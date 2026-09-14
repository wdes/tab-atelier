// SPDX-License-Identifier: MPL-2.0

//! Keys: minting one, and switching one off.

use super::{Rejection, Validated, loose_bool};

/// Minting a named key.
///
/// There is no field for the secret, because the secret is generated. Accepting
/// one would let a caller store a credential somebody else already knows, and
/// the response to this request is the only place a secret is ever shown.
///
/// An unnamed key is called `new`: the name is the operator's handle for the
/// key, and an empty one leaves a row they cannot tell apart from the next.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AddKey {
    #[serde(default)]
    pub name: String,
}

impl AddKey {
    /// The name to store, with the empty string replaced.
    #[must_use]
    pub fn name(&self) -> &str {
        if self.name.trim().is_empty() {
            "new"
        } else {
            self.name.as_str()
        }
    }
}

impl Validated for AddKey {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }

    fn accept(body: &bytes::Bytes) -> Result<Self, Rejection> {
        super::parse(body)
    }
}

/// Switching one key off, or back on.
///
/// The destructive reading is the default. The route is reached by asking to
/// disable a key, so a body that does not say otherwise means it — and that is
/// why the field is an [`Option`] rather than a defaulted `false`, which would
/// silently mean the opposite.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SetKeyDisabled {
    #[serde(default, deserialize_with = "loose_bool::optional")]
    pub disabled: Option<bool>,
}

impl SetKeyDisabled {
    /// The switch the request asked for.
    #[must_use]
    pub fn disabled(&self) -> bool {
        self.disabled.unwrap_or(true)
    }
}

impl Validated for SetKeyDisabled {
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

    #[test]
    fn a_key_with_no_name_is_called_new() {
        let key = AddKey::accept(&Bytes::from_static(b"{}")).expect("valid");
        assert_eq!(key.name(), "new");
        let named = AddKey::accept(&Bytes::from_static(br#"{"name":"laptop"}"#)).expect("valid");
        assert_eq!(named.name(), "laptop");
    }

    #[test]
    fn an_empty_name_is_the_same_as_no_name() {
        let key = AddKey::accept(&Bytes::from_static(br#"{"name":"  "}"#)).expect("valid");
        assert_eq!(key.name(), "new");
    }

    /// Absent means disable — the route's own verb — and only `false` restores.
    #[test]
    fn disabling_is_what_silence_means() {
        let silent = SetKeyDisabled::accept(&Bytes::from_static(b"{}")).expect("valid");
        assert!(silent.disabled());
        let restore = SetKeyDisabled::accept(&Bytes::from_static(br#"{"disabled":false}"#)).expect("valid");
        assert!(!restore.disabled());
        let explicit = SetKeyDisabled::accept(&Bytes::from_static(br#"{"disabled":true}"#)).expect("valid");
        assert!(explicit.disabled());
    }

    #[test]
    fn a_key_body_that_is_not_json_is_refused() {
        assert_eq!(AddKey::accept(&Bytes::from_static(b"nope")).unwrap_err().status, 400);
    }
}
