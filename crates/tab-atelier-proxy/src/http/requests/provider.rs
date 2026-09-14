// SPDX-License-Identifier: MPL-2.0

//! Saving a provider: adding one, editing one, or copying one.

use crate::provider::Preset;

use super::{Validated, loose_bool};

/// A provider as the form sends it.
///
/// Everything is `#[serde(default)]` because the same body covers three
/// different intentions — naming a preset, describing a provider by hand, and
/// editing an existing row — and each names a different subset. What must be
/// present is not "every field" but "enough to do the thing asked", which is a
/// rule about the values together, so it lives in [`Validated::validate`].
///
/// `dup` and `enabled` are [`Option`]s rather than defaulted booleans, and the
/// distinction is load-bearing: an absent `enabled` means *leave the current
/// value alone* while `false` means *switch it off*. Collapsing the two is the
/// bug this route already had once — see `save_provider`.
#[derive(Debug, serde::Deserialize)]
pub struct SaveProvider {
    /// One of [`Preset::ALL`], or empty for a hand-written provider.
    #[serde(default)]
    pub preset: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub base_url: String,
    /// Newline-separated model names, as typed into the form's textarea.
    #[serde(default)]
    pub models: String,
    /// The upstream secret. Empty on an edit that is not changing it.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub preference: Option<i32>,
    /// Keep the existing row and add this as a second provider beside it.
    #[serde(default, deserialize_with = "loose_bool::optional")]
    pub dup: Option<bool>,
    #[serde(default, deserialize_with = "loose_bool::optional")]
    pub enabled: Option<bool>,
}

impl SaveProvider {
    /// The preset that was named, if one was.
    #[must_use]
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL.iter().find(|p| p.id() == self.preset).copied()
    }

    /// Whether the caller asked for a copy rather than an edit.
    #[must_use]
    pub fn duplicate(&self) -> bool {
        self.dup.unwrap_or(false)
    }

    /// The rotation weight, defaulting to the form's own ten.
    #[must_use]
    pub fn preference(&self) -> i32 {
        self.preference.unwrap_or(10)
    }
}

impl Validated for SaveProvider {
    fn validate(&self) -> Result<(), String> {
        // A named preset carries its own id and base URL, so neither is asked
        // for. An unrecognised preset name is not an error in itself: it falls
        // through to the hand-written path below and is refused there for the
        // field it actually lacks, which is the more useful message.
        if self.preset().is_some() {
            return Ok(());
        }
        if self.id.trim().is_empty() {
            return Err("a provider needs an id".to_owned());
        }
        if self.base_url.trim().is_empty() {
            return Err("a provider needs a base_url".to_owned());
        }
        Ok(())
    }
}

/// Replacing one provider's key, and nothing else.
#[derive(Debug, serde::Deserialize)]
pub struct RotateProviderKey {
    #[serde(default)]
    pub key: String,
}

impl Validated for RotateProviderKey {
    fn validate(&self) -> Result<(), String> {
        if self.key.trim().is_empty() {
            return Err("no key given".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn parse(json: &str) -> Result<SaveProvider, u16> {
        SaveProvider::accept(&Bytes::copy_from_slice(json.as_bytes())).map_err(|r| r.status)
    }

    #[test]
    fn a_preset_needs_nothing_else() {
        assert!(parse(r#"{"preset":"deepseek"}"#).is_ok());
    }

    #[test]
    fn a_hand_written_provider_needs_an_id_and_a_base() {
        assert!(parse(r#"{"id":"a","base_url":"https://x/v1"}"#).is_ok());
        assert_eq!(parse(r#"{"base_url":"https://x/v1"}"#).unwrap_err(), 400);
        assert_eq!(parse(r#"{"id":"a"}"#).unwrap_err(), 400);
    }

    #[test]
    fn an_unknown_preset_falls_through_to_the_hand_written_rules() {
        // Not "unknown preset": that is less useful than saying which field is
        // missing to make it a provider.
        assert_eq!(parse(r#"{"preset":"nope","id":"a"}"#).unwrap_err(), 400);
        assert!(parse(r#"{"preset":"nope","id":"a","base_url":"https://x/v1"}"#).is_ok());
    }

    /// The distinction the route depends on: absent is not the same as false.
    #[test]
    fn an_absent_flag_is_not_a_decision_and_a_present_one_is() {
        let quiet = parse(r#"{"preset":"deepseek"}"#).expect("valid");
        assert_eq!(quiet.enabled, None, "absent keeps the current value");
        assert_eq!(quiet.dup, None);

        let loud = parse(r#"{"preset":"deepseek","enabled":false,"dup":true}"#).expect("valid");
        assert_eq!(loud.enabled, Some(false));
        assert!(loud.duplicate());
    }

    /// The old form sent flags as strings, and a body from one is still live
    /// against a rolling deployment. Both spellings mean the same thing.
    #[test]
    fn a_flag_spelled_as_a_string_still_reads() {
        let s = parse(r#"{"preset":"deepseek","enabled":"false","dup":"true"}"#).expect("valid");
        assert_eq!(s.enabled, Some(false));
        assert!(s.duplicate());
        // And anything else is a type error rather than a silent false.
        assert_eq!(parse(r#"{"preset":"deepseek","enabled":"yes"}"#).unwrap_err(), 400);
    }

    #[test]
    fn the_preference_defaults_to_ten() {
        assert_eq!(parse(r#"{"preset":"deepseek"}"#).expect("valid").preference(), 10);
        assert_eq!(
            parse(r#"{"preset":"deepseek","preference":3}"#)
                .expect("valid")
                .preference(),
            3
        );
    }

    #[test]
    fn a_rotation_without_a_key_is_refused() {
        assert_eq!(
            RotateProviderKey::accept(&Bytes::from_static(b"{}"))
                .unwrap_err()
                .status,
            400
        );
        assert!(RotateProviderKey::accept(&Bytes::from_static(br#"{"key":"sk"}"#)).is_ok());
    }
}
