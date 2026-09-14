// SPDX-License-Identifier: MPL-2.0

//! Accounts: creating one, and the four per-account switches.

use super::{Validated, loose_bool};

/// Creating an account.
///
/// None of the three fields is required, which is why there is no rule here:
/// `Store::add` accepts the empty string for any of them and folds an empty
/// email into an internal id. Refusing an incomplete person would be a
/// policy the store does not have.
#[derive(Debug, Default, serde::Deserialize)]
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
}

/// Pinning an account to one provider, or clearing the pin.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PinProvider {
    /// Empty clears the pin, which is why an absent field is not an error.
    #[serde(default)]
    pub provider: String,
}

impl Validated for PinProvider {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Pinning an account to one model name, or clearing the pin.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PinModel {
    #[serde(default)]
    pub model: String,
}

impl Validated for PinModel {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Switching an account off, or back on.
#[derive(Debug, Default, serde::Deserialize)]
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
}

/// How many times this account's key is drawn in the rotation.
///
/// The weight is clamped by the store, not here, so that the floor lives in one
/// place no matter which caller writes it.
#[derive(Debug, Default, serde::Deserialize)]
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

    #[test]
    fn an_absent_weight_is_one() {
        let d: SetWeight = parse("{}");
        assert_eq!(d.weight(), 1);
        let w: SetWeight = parse(r#"{"weight":7}"#);
        assert_eq!(w.weight(), 7);
    }
}
