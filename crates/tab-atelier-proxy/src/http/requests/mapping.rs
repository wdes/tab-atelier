// SPDX-License-Identifier: MPL-2.0

//! Model-name mappings.

use super::Validated;

/// A mapping from a name a client asks for to the name the provider serves.
///
/// Applied globally before routing, so one entry changes what every account
/// gets. The emptiness of either end is a rule about the pair, and it is
/// checked here rather than in the handler because a mapping with a blank half
/// has no meaning to hand downstream at all.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AddMapping {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    /// Free text, shown in the table. Never read by routing.
    #[serde(default)]
    pub note: String,
}

impl Validated for AddMapping {
    fn validate(&self) -> Result<(), String> {
        if self.from.trim().is_empty() {
            return Err("a mapping needs a `from` model".to_owned());
        }
        if self.to.trim().is_empty() {
            return Err("a mapping needs a `to` model".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn parse(json: &str) -> Result<AddMapping, u16> {
        AddMapping::accept(&Bytes::copy_from_slice(json.as_bytes())).map_err(|r| r.status)
    }

    #[test]
    fn both_ends_are_required() {
        assert!(parse(r#"{"from":"a","to":"b"}"#).is_ok());
        assert_eq!(parse(r#"{"to":"b"}"#).unwrap_err(), 400);
        assert_eq!(parse(r#"{"from":"a"}"#).unwrap_err(), 400);
        assert_eq!(parse("{}").unwrap_err(), 400);
    }

    #[test]
    fn whitespace_is_not_a_model_name() {
        assert_eq!(parse(r#"{"from":"  ","to":"b"}"#).unwrap_err(), 400);
    }

    #[test]
    fn the_note_is_optional_and_never_routing() {
        let m = parse(r#"{"from":"a","to":"b"}"#).expect("valid");
        assert_eq!(m.note, "");
        let noted = parse(r#"{"from":"a","to":"b","note":"why"}"#).expect("valid");
        assert_eq!(noted.note, "why");
    }
}
