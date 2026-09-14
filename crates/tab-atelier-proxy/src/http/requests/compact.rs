// SPDX-License-Identifier: MPL-2.0

//! An account's compaction level.

use crate::compact::Compact;

use super::Validated;

/// Setting how much of an account's conversation the proxy may shed.
///
/// The level arrives as a string and is resolved against [`Compact::ALL`] here
/// rather than deserialised into the enum directly. That is deliberate: the
/// level names are a wire format the UI and `providers.json` share, and an
/// unrecognised one should be refused with the name the caller actually sent —
/// which is the one thing a serde derive cannot put in the message.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SetCompact {
    #[serde(default)]
    pub compact: String,
}

impl SetCompact {
    /// The level that was named, or [`None`] for one this proxy does not have.
    ///
    /// Trimmed, because the form is a `<select>` in the browser but the CLI is
    /// not, and a pasted newline is the common way to get this wrong.
    #[must_use]
    pub fn level(&self) -> Option<Compact> {
        let wanted = self.compact.trim();
        Compact::ALL.into_iter().find(|c| c.as_str() == wanted)
    }
}

impl Validated for SetCompact {
    fn validate(&self) -> Result<(), String> {
        match self.level() {
            Some(_) => Ok(()),
            None => Err(format!("unknown compaction level {:?}", self.compact)),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn parse(json: &str) -> Result<SetCompact, u16> {
        SetCompact::accept(&Bytes::copy_from_slice(json.as_bytes())).map_err(|r| r.status)
    }

    #[test]
    fn every_level_the_enum_knows_is_accepted() {
        for level in Compact::ALL {
            let body = format!(r#"{{"compact":{:?}}}"#, level.as_str());
            let parsed = parse(&body).unwrap_or_else(|e| panic!("{level:?} refused with {e}"));
            assert_eq!(parsed.level(), Some(level));
        }
    }

    #[test]
    fn an_unknown_level_is_refused_and_named() {
        let parsed = SetCompact::accept(&Bytes::from_static(br#"{"compact":"everything"}"#)).expect_err("must refuse");
        assert_eq!(parsed.status, 400);
        assert!(
            parsed.body_contains("everything"),
            "the message must quote the level that was sent"
        );
    }

    #[test]
    fn whitespace_around_a_real_level_is_tolerated() {
        let parsed = parse(r#"{"compact":" tools "}"#).expect("valid");
        assert_eq!(parsed.level(), Some(Compact::Tools));
    }

    #[test]
    fn an_absent_level_is_refused_rather_than_silently_none() {
        // "none" is a real level and a missing field is not it: defaulting here
        // would turn a malformed request into a policy change.
        assert_eq!(parse("{}").unwrap_err(), 400);
    }
}
