// SPDX-License-Identifier: MPL-2.0

//! A request body is a struct that parses and validates itself — and the
//! envelope that body arrives in.
//!
//! A handler never sees a raw body. It takes a type that could only have been
//! built from a body that was valid — the parsing, the presence check and the
//! cross-field rules are all in [`Validated::validate`], so by the time a
//! controller has a value there is nothing left to be defensive about. The
//! refusal happens once, here, and every handler below it assumes the happy
//! path.
//!
//! [`Request`] is the rest of what a handler is given: the method, the path, the
//! query and the caching headers, as a transport parsed them. It names no
//! framework, so the same handler serves a Rocket route and a server that owns
//! its listener.
//!
//! # Adding a request
//!
//! 1. Derive `serde::Deserialize` and `schemars::JsonSchema`, marking every
//!    field `#[serde(default)]` unless it is genuinely required (the schema is
//!    what documents the body, so the two derives travel together).
//! 2. Implement [`Validated`] with the rules that span fields.
//! 3. Give any field whose absence differs from its zero value an [`Option`],
//!    with a named accessor that states what silence means.

use bytes::Bytes;

use crate::cache::Conditional;
use crate::marshal::{Reply, json_of};
use crate::resource::ProblemResource;

/// What a handler is handed, once a transport has parsed the wire.
///
/// The body contract below is half of it; this is the other half. A server that
/// keeps its own listener — the daemon does, for its WebSocket viewer and SSE
/// relay — parses HTTP/1.1 into one of these and hands it to the same handlers a
/// Rocket route would call, so a route is written once instead of twice. The
/// type names no framework, which is what makes that possible.
///
/// Copy, because it borrows: a handler that needs to hold a piece of the request
/// past the call borrows the piece, not the envelope.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The method, as the wire spelled it — upper-case from a client, but a
    /// handler compares rather than assumes (`HEAD` is normalised to `GET` by
    /// the transport, which is where that convention belongs).
    pub method: &'a str,
    /// The path, with the query string and the fragment already removed.
    pub path: &'a str,
    /// The raw query string, without the leading `?`. Empty when there was none.
    pub query: &'a str,
    /// The body, empty for a method that carries none.
    pub body: &'a Bytes,
    /// What this request said about caching: its `Accept-Encoding` and
    /// `If-None-Match`, already read.
    pub conditional: Conditional<'a>,
}

impl<'a> Request<'a> {
    /// A request with no query and no body, for a fixed-path GET.
    #[must_use]
    pub const fn get(path: &'a str) -> Self {
        // A `static` rather than a temporary: `&Bytes::new()` would need the
        // temporary promoted to `'static`, which the compiler refuses for a
        // type with a destructor. `Bytes::new()` is const, so this is free.
        static EMPTY: Bytes = Bytes::new();
        Self {
            method: "GET",
            path,
            query: "",
            body: &EMPTY,
            conditional: Conditional::none(),
        }
    }

    /// The raw value of `name` in the query string, still percent-encoded.
    ///
    /// Raw on purpose: whether a value needs decoding depends on what it means,
    /// and a path arrives already decoded by the time it is matched. The first
    /// occurrence wins, as RFC 3986 leaves duplicates undefined and a caller that
    /// cares should not be reading a second one.
    #[must_use]
    pub fn query_value(&self, name: &str) -> Option<&'a str> {
        self.query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    }
}

/// The deserialiser half of [`Validated`] for a handler holding a [`Request`].
///
/// # Errors
///
/// [`Rejection`], exactly as [`Validated::accept`] documents — this is a
/// convenience over it so a handler passes its envelope rather than the body
/// field.
pub fn body_of<T: Validated>(request: &Request<'_>) -> Result<T, Rejection> {
    T::accept(request.body)
}

/// A request body that refuses to become a value when it is not valid.
///
/// The two rules are separate steps for a reason: a body that is not JSON at all
/// and a body that is JSON but says something impossible are different failures,
/// and the second should be able to say which field it is unhappy with. `serde`
/// reports the first; [`Validated::validate`] reports the second.
pub trait Validated: Sized {
    /// The rules that span more than one field.
    ///
    /// Returns the sentence to show the caller on refusal. Field-level shape — a
    /// number where a string belongs, a missing key — is serde's job and never
    /// reaches here.
    ///
    /// # Errors
    ///
    /// The sentence to show the caller, when the body parsed but says something
    /// impossible.
    fn validate(&self) -> Result<(), String>;

    /// Parse and validate in one step, as a controller would.
    ///
    /// A request whose wire shape is not its struct shape implements this
    /// directly instead of bending the struct to fit whichever caller was
    /// written first. Every other request forwards to [`parse`].
    ///
    /// # Errors
    ///
    /// [`Rejection`], because the body is not valid JSON for this type or
    /// because [`Validated::validate`] refused it.
    fn accept(body: &Bytes) -> Result<Self, Rejection>;
}

/// The `serde` half of [`Validated::accept`], for the requests whose wire shape
/// is their struct shape.
///
/// A free function rather than a default method so a request with a bespoke body
/// is not forced to implement `Deserialize` just to satisfy a bound it never
/// uses.
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
    /// The HTTP status. 400 for everything this layer produces today; the field
    /// exists so a rule that means 404/409/422 can say so rather than being
    /// flattened into a 400.
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
    /// The field is named in the sentence because a form with eight inputs and a
    /// bare "invalid" is a guessing game; the caller has somewhere to put the
    /// answer, so the answer says where.
    #[must_use]
    pub fn field(name: &str, why: &str) -> Self {
        Self {
            status: 400,
            message: format!("{name}: {why}"),
        }
    }

    /// A refusal with a status other than 400 — a rule that knows the thing it
    /// names does not exist (404), or cannot be reconciled with what is stored
    /// (409).
    #[must_use]
    pub const fn with_status(status: u16, message: String) -> Self {
        Self { status, message }
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
        json_of(r.status, &ProblemResource::of(r.message))
    }
}

/// Deserialisers for the flags that predate strict typing on an API.
///
/// An old form posted `"true"`/`"false"` as strings, and a body from a page that
/// has not reloaded is still live against a rolling deployment. Accepting both
/// spellings costs one function; rejecting the string would turn a working save
/// into a 400 for the duration of a deploy.
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
    /// If the field is present but neither a boolean nor one of the two strings
    /// — silently reading that as `false` is how a flag goes missing.
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

    use super::{Rejection, Reply, Request, Validated, parse};

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

    #[test]
    fn a_rule_can_name_a_status_other_than_400() {
        let r = Rejection::with_status(409, "already exists".to_owned());
        let reply: Reply = r.into();
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn a_query_value_is_found_by_its_exact_name() {
        // `token` must not match `tokens`, which is why the name is matched
        // with its `=` rather than as a prefix of the pair.
        let request = Request {
            query: "lines=40&token=abc&tokens=no",
            ..Request::get("/tabs")
        };
        assert_eq!(request.query_value("token"), Some("abc"));
        assert_eq!(request.query_value("lines"), Some("40"));
        assert_eq!(request.query_value("nothing"), None);
    }

    #[test]
    fn a_get_has_no_query_no_body_and_no_caching_opinion() {
        let request = Request::get("/tabs");
        assert_eq!(request.method, "GET");
        assert!(request.body.is_empty());
        assert_eq!(request.query_value("anything"), None);
        assert_eq!(request.conditional.if_none_match, None);
    }
}
