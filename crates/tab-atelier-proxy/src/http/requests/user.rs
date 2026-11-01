// SPDX-License-Identifier: MPL-2.0

//! Accounts: creating one, and the four per-account switches.

use super::{Rejection, Validated, loose_bool};
use tab_atelier_api::schemars;

/// Creating an account.
///
/// None of the three fields is required, which is why there is no rule here:
/// `Store::add` accepts the empty string for any of them and folds an empty
/// email into an internal id. Refusing an incomplete person would be a
/// policy the store does not have.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct AddUser {
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub email: String,
}

impl Validated for AddUser {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

/// Pinning an account to one provider, or clearing the pin.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct PinProvider {
    /// Empty clears the pin, which is why an absent field is not an error.
    #[serde(default)]
    pub provider: String,
}

impl Validated for PinProvider {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

/// Pinning an account to one model name, or clearing the pin.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct PinModel {
    #[serde(default)]
    pub model: String,
}

impl Validated for PinModel {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

/// Declaring what kind of traffic an account produces.
///
/// `kind` empty clears the override. The values are the same spellings routes
/// accept — `work`, `classifier`/`auto`, `plan`/`plan_mode` — so one vocabulary
/// covers both.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct SetKind {
    #[serde(default)]
    pub kind: String,
}

impl Validated for SetKind {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        // Parsed and validated at the boundary, like every other request here:
        // `validate` is a hook nothing calls unless `accept` does.
        let parsed: Self = super::parse(body)?;
        parsed.validate().map_err(|why| Rejection::field("kind", &why))?;
        Ok(parsed)
    }
}

/// Adding or replacing one of an account's per-person model routes.
///
/// `to_model` empty means "delete the route for `from`", mirroring how an empty
/// `model` clears the pin: one endpoint, and the operator does not have to know
/// a second verb to undo what they just did.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct SetRoute {
    /// The model id this account asks for, or empty for any model.
    ///
    /// One of the three conditions. A route with none of them would match every
    /// request, which is a configuration mistake rather than a rule.
    #[serde(default)]
    pub from: String,
    /// Which traffic: `work`, or `classifier` (also accepted: `auto`).
    ///
    /// Empty means work only — the auto-mode judgement is never moved by a route
    /// that did not name it.
    #[serde(default)]
    pub kind: String,
    /// Only reroute traffic that was heading for this provider.
    #[serde(default)]
    pub from_provider: String,
    /// Only reroute traffic that declares this server-side tool.
    ///
    /// `web_search` or `web_fetch`. A versioned type is accepted and normalised
    /// to the family, so a rule written today keeps applying when the version
    /// moves.
    #[serde(default)]
    pub tool: String,
    /// The model id to serve instead, or empty to remove the route.
    #[serde(default)]
    pub to_model: String,
    /// The provider to serve it from, or absent for whichever one does.
    #[serde(default)]
    pub to_provider: Option<String>,
    /// Why, for whoever reads the file later.
    #[serde(default)]
    pub note: Option<String>,
}

impl Validated for SetRoute {
    fn validate(&self) -> Result<(), String> {
        // At least one condition, or the rule matches everything — which a
        // provider pin plus a model pin already expresses, and which as a route
        // would silently swallow every request including the auto-mode judge.
        let removing = self.to_model.trim().is_empty();
        if !removing
            && self.from.trim().is_empty()
            && self.kind.trim().is_empty()
            && self.from_provider.trim().is_empty()
        {
            return Err("route: give at least one of from, kind or from_provider, or it matches everything".to_owned());
        }
        // A route to the same name is a no-op that looks like a configured
        // one, which is worse than a refusal — the operator would believe
        // something was in effect.
        if !self.to_model.trim().is_empty() && self.from.trim().eq_ignore_ascii_case(self.to_model.trim()) {
            return Err("to_model: a route to the same model changes nothing".to_owned());
        }
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        // Validated here, in `accept`, and not only in `validate` — the trait
        // method is a hook nobody calls unless the impl does. `SetTools` is the
        // precedent. This is the same class of gap as a rule written down and
        // never enforced, which is exactly the bug that brought this file into
        // being.
        let parsed: Self = super::parse(body)?;
        parsed.validate().map_err(|why| Rejection::field("route", &why))?;
        Ok(parsed)
    }
}

/// Switching an account off, or back on.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct SetDisabled {
    #[serde(default, deserialize_with = "loose_bool::optional")]
    pub disabled: Option<bool>,
}

impl SetDisabled {
    /// Absent means disable: the route's verb is the default reading.
    #[must_use]
    pub fn disabled(&self) -> bool {
        self.disabled.unwrap_or(true)
    }
}

impl Validated for SetDisabled {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

/// The account's tool policy.
///
/// Read in either shape the callers use: the UI sends `{"tools": {...}}` and a
/// script tends to send the policy bare. The envelope is checked first, because
/// a bare policy deserialises from `{"tools": ...}` too — to all defaults —
/// which would silently discard everything the caller asked for.
#[derive(Debug, tab_atelier_api::schemars::JsonSchema)]
pub struct SetTools {
    pub policy: crate::tools::Policy,
}

impl Validated for SetTools {
    fn validate(&self) -> Result<(), String> {
        crate::tools::validate(&self.policy)
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| Rejection::field("tools", &format!("not JSON: {e}")))?;
        let inner = value.get("tools").cloned().unwrap_or(value);
        let policy: crate::tools::Policy =
            serde_json::from_value(inner).map_err(|e| Rejection::field("tools", &format!("bad tool policy: {e}")))?;
        let parsed = Self { policy };
        parsed.validate().map_err(|why| Rejection::field("tools", &why))?;
        Ok(parsed)
    }
}

/// How many times this account's key is drawn in the rotation.
///
/// The weight is clamped by the store, not here, so that the floor lives in one
/// place no matter which caller writes it.
#[derive(Debug, Default, serde::Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct SetWeight {
    #[serde(default)]
    pub weight: Option<u32>,
}

impl SetWeight {
    /// Absent means the default weight of one, matching the form's own default.
    #[must_use]
    pub fn weight(&self) -> u32 {
        self.weight.unwrap_or(1)
    }
}

impl Validated for SetWeight {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn parse<T: Validated + serde::de::DeserializeOwned>(json: &str) -> T {
        T::accept(&Bytes::copy_from_slice(json.as_bytes())).expect("valid")
    }

    #[test]
    fn an_incomplete_person_is_still_a_person() {
        let bare: AddUser = parse("{}");
        assert_eq!(bare.first_name, "");
        let full: AddUser = parse(r#"{"first_name":"A","last_name":"B","email":"a@b"}"#);
        assert_eq!(full.email, "a@b");
    }

    #[test]
    fn an_absent_pin_means_no_pin() {
        let p: PinProvider = parse("{}");
        assert_eq!(p.provider, "");
        let m: PinModel = parse("{}");
        assert_eq!(m.model, "");
    }

    #[test]
    fn silence_disables_an_account_and_only_false_restores_it() {
        let silent: SetDisabled = parse("{}");
        assert!(silent.disabled());
        let off: SetDisabled = parse(r#"{"disabled":false}"#);
        assert!(!off.disabled());
    }

    /// Both shapes the callers send must read as the same policy.
    ///
    /// The envelope is checked first for a reason: read as a bare policy,
    /// `{"tools": …}` deserialises to all defaults and discards what the caller
    /// asked for — so an operator restricting an account would watch the
    /// restriction vanish and the account keep the permissive default.
    #[test]
    fn a_tools_body_is_read_in_either_shape() {
        for body in [
            r#"{"mode":"none","disable":["Read"]}"#,
            r#"{"tools":{"mode":"none","disable":["Read"]}}"#,
        ] {
            let parsed = SetTools::accept(&Bytes::copy_from_slice(body.as_bytes())).expect("valid");
            assert_eq!(parsed.policy.mode, crate::tools::Mode::None, "{body}");
            assert_eq!(parsed.policy.disable, vec!["Read".to_string()], "{body}");
        }
    }

    /// A body that is not a policy is refused rather than defaulted.
    ///
    /// A default `Policy` allows everything, so reading an unreadable body as
    /// one would turn a malformed request into the most permissive answer
    /// possible — the opposite of what a request that failed to parse should do.
    #[test]
    fn a_tools_body_that_is_not_a_policy_is_refused() {
        for body in ["\"nope\"", "[1,2]", "null", "7"] {
            assert!(
                SetTools::accept(&Bytes::copy_from_slice(body.as_bytes())).is_err(),
                "{body} was accepted"
            );
        }
    }

    #[test]
    fn a_tools_body_that_is_not_json_is_refused() {
        // The rejection names the field so the operator knows which part of the
        // body to look at.
        let refused = SetTools::accept(&Bytes::from_static(b"{not json")).expect_err("refused");
        assert_eq!(refused.status, 400);
        assert!(refused.message.contains("tools"), "{}", refused.message);
    }

    #[test]
    fn an_absent_weight_is_one() {
        let d: SetWeight = parse("{}");
        assert_eq!(d.weight(), 1);
        let w: SetWeight = parse(r#"{"weight":7}"#);
        assert_eq!(w.weight(), 7);
    }
}
