// SPDX-License-Identifier: MPL-2.0

//! One struct per request body, each of which parses and validates itself.
//!
//! A handler in this tree never sees a raw body. It takes a type that could
//! only have been built from a body that was valid — the parsing, the presence
//! check and the cross-field rules are all in `validate`, so by the time a
//! controller has a value there is nothing left to be defensive about. That is
//! the whole point of the layer: the refusal happens once, here, and every
//! handler below it can assume the happy path.
//!
//! Bodies are deserialised into structs by serde; there is no hand-written JSON
//! walking anywhere in this module or below it.
//!
//! # Adding a request
//!
//! 1. Derive `serde::Deserialize`, marking every field `#[serde(default)]` or
//!    `#[serde(default = "...")]` unless it is genuinely required.
//! 2. Implement [`Validated`] with the rules that span fields.
//! 3. Give any field whose absence differs from its zero value an [`Option`],
//!    with a named accessor that states what silence means — the difference
//!    between "absent" and "false" has caused a bug on this API before.

pub mod compact;
pub mod inspect;
pub mod key;
pub mod mapping;
pub mod provider;
pub mod user;

pub use compact::SetCompact;
pub use inspect::ArmInspect;
pub use key::{AddKey, SetKeyDisabled};
pub use mapping::AddMapping;
pub use provider::{RotateProviderKey, SaveProvider};
pub use user::{AddUser, PinModel, PinProvider, SetDisabled, SetWeight};

use bytes::Bytes;

use crate::transport::{Reply, json_of};

/// A request body that refuses to become a value when it is not valid.
///
/// The two rules are separate steps for a reason: a body that is not JSON at
/// all and a body that is JSON but says something impossible are different
/// failures, and the second should be able to say which field it is unhappy
/// with. `serde` reports the first; `validate` reports the second.
pub trait Validated: Sized {
    /// The rules that span more than one field.
    ///
    /// Returns the sentence to show the caller on refusal. Field-level shape —
    /// a number where a string belongs, a missing key — is serde's job and
    /// never reaches here.
    ///
    /// # Errors
    ///
    /// The sentence to show the caller, when the body parsed but says
    /// something impossible.
    fn validate(&self) -> Result<(), String>;

    /// Parse and validate in one step, as a controller would.
    ///
    /// A request whose wire shape is not its struct shape — the tool policy,
    /// which arrives either wrapped or bare — implements this directly instead
    /// of bending the struct to fit whichever caller was written first. Every
    /// other request forwards to [`parse`], which is this same default with the
    /// `serde` bound made explicit.
    ///
    /// # Errors
    ///
    /// [`Rejection`] with 400, either because the body is not valid JSON for
    /// this type or because [`Validated::validate`] refused it.
    fn accept(body: &Bytes) -> Result<Self, Rejection>;
}

/// The `serde` half of [`Validated::accept`], for the requests whose wire shape
/// is their struct shape.
///
/// It is a free function rather than a default method so that a request with a
/// bespoke body — the tool policy, which may arrive wrapped or bare — is not
/// forced to pretend it implements `Deserialize` just to satisfy a bound it
/// never uses.
///
/// # Errors
///
/// [`Rejection`] with 400, naming the malformed field when `serde` can, or the
/// rule that was broken when [`Validated::validate`] can.
pub fn parse<T>(body: &Bytes) -> Result<T, Rejection>
where
    T: Validated + serde::de::DeserializeOwned,
{
    let parsed: T = serde_json::from_slice(body).map_err(|e| Rejection::malformed(&e))?;
    parsed.validate().map_err(Rejection::refused)?;
    Ok(parsed)
}

/// A refused request body, ready to become the response.
///
/// Carries the sentence rather than a status alone so the caller can be told
/// what to change — a bare 400 on a form with eight fields is a guessing game.
#[derive(Debug)]
pub struct Rejection {
    /// The HTTP status: 400 for everything this layer produces.
    pub status: u16,
    /// What was wrong, as it will be shown to the caller.
    pub message: String,
}

impl Rejection {
    /// A body that could not be read as this request at all.
    fn malformed(error: &serde_json::Error) -> Self {
        Self {
            status: 400,
            message: error.to_string(),
        }
    }

    /// A body that parsed but broke a rule.
    const fn refused(why: String) -> Self {
        Self {
            status: 400,
            message: why,
        }
    }

    /// A body that broke a rule about one named field.
    ///
    /// The field is named in the sentence because a form with eight inputs and
    /// a bare "invalid" is a guessing game; the caller has somewhere to put
    /// the answer, so the answer says where.
    #[must_use]
    pub fn field(name: &str, why: &str) -> Self {
        Self {
            status: 400,
            message: format!("{name}: {why}"),
        }
    }

    /// Whether the refusal mentions `needle` — for asserting that a message
    /// names the field it is about, not merely that it has a nonzero length.
    #[must_use]
    pub fn body_contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
    }
}

impl From<Rejection> for Reply {
    fn from(r: Rejection) -> Self {
        json_of(
            r.status,
            &crate::http::resources::status::ProblemResource::of(r.message),
        )
    }
}

/// Deserialisers for the flags that predate strict typing on this API.
///
/// The old form posted `"true"`/`"false"` as strings, and a body from a page
/// that has not reloaded is still live against a rolling deployment. Accepting
/// both spellings costs one function; rejecting the string would turn a
/// working save into a 400 for the duration of a deploy.
pub mod loose_bool {
    use serde::de::{self, Deserializer, Unexpected, Visitor};
    use std::fmt;

    struct Loose;

    impl Visitor<'_> for Loose {
        type Value = bool;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean or the strings \"true\"/\"false\"")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
            match v {
                "true" => Ok(true),
                "false" => Ok(false),
                other => Err(E::invalid_value(Unexpected::Str(other), &self)),
            }
        }
    }

    /// The boolean a named field holds, or `None` if the field is absent.
    ///
    /// # Errors
    ///
    /// If the field is present but neither a boolean nor one of the two
    /// strings — silently reading that as `false` is how a flag goes missing.
    pub fn optional<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        d.deserialize_option(LooseOption)
    }

    struct LooseOption;

    impl<'de> Visitor<'de> for LooseOption {
        type Value = Option<bool>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean, or none")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(Loose).map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    /// A request whose only job is to refuse, so the trait's two failure modes
    /// can be told apart without a real DTO in the way.
    #[derive(Debug, serde::Deserialize)]
    struct OnlyTwo {
        #[serde(default)]
        n: u32,
    }

    impl Validated for OnlyTwo {
        fn validate(&self) -> Result<(), String> {
            if self.n > 2 {
                Err(format!("n must be at most 2, got {}", self.n))
            } else {
                Ok(())
            }
        }

        fn accept(body: &Bytes) -> Result<Self, Rejection> {
            parse(body)
        }
    }

    #[test]
    fn a_valid_body_becomes_a_value() {
        let v = OnlyTwo::accept(&Bytes::from_static(br#"{"n":1}"#)).expect("valid");
        assert_eq!(v.n, 1);
    }

    #[test]
    fn json_of_the_wrong_shape_is_refused_as_malformed() {
        let r = OnlyTwo::accept(&Bytes::from_static(br#"{"n":"one"}"#)).unwrap_err();
        assert_eq!(r.status, 400);
        assert!(r.body_contains("invalid type"), "serde's own wording");
    }

    #[test]
    fn a_rule_breach_is_refused_with_the_rule_s_own_sentence() {
        let r = OnlyTwo::accept(&Bytes::from_static(br#"{"n":99}"#)).unwrap_err();
        assert_eq!(r.status, 400);
        assert!(r.body_contains("at most 2"), "the rule, not serde's wording");
    }

    #[test]
    fn a_defaulted_field_may_be_absent() {
        let v = OnlyTwo::accept(&Bytes::from_static(b"{}")).expect("valid");
        assert_eq!(v.n, 0);
    }

    #[test]
    fn a_refusal_becomes_the_response_unchanged() {
        let r = OnlyTwo::accept(&Bytes::from_static(br#"{"n":99}"#)).unwrap_err();
        let reply: Reply = r.into();
        assert_eq!(reply.status, 400);
    }
}
