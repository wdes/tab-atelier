// SPDX-License-Identifier: MPL-2.0

//! Arming request inspection.

use super::Validated;

/// How long to capture requests for.
///
/// The default is fifteen minutes because arming is a debugging act, not a
/// setting: the useful window is the one in which you reproduce the thing, and
/// an inspection left on is a log of other people's prompts. `minutes` is
/// clamped by [`crate::inspect`] as well, so a hand-rolled request cannot
/// outlive the ceiling the UI respects.
#[derive(Debug, serde::Deserialize)]
pub struct ArmInspect {
    #[serde(default = "default_minutes")]
    pub minutes: u64,
}

const fn default_minutes() -> u64 {
    15
}

impl ArmInspect {
    /// The requested window, in minutes.
    #[must_use]
    pub const fn minutes(&self) -> u64 {
        self.minutes
    }
}

impl Validated for ArmInspect {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn parse(json: &str) -> ArmInspect {
        ArmInspect::accept(&Bytes::copy_from_slice(json.as_bytes())).expect("valid")
    }

    #[test]
    fn silence_arms_for_the_default_window() {
        assert_eq!(parse("{}").minutes(), 15);
    }

    #[test]
    fn an_explicit_window_is_honoured() {
        assert_eq!(parse(r#"{"minutes":45}"#).minutes(), 45);
    }

    /// Zero is a real request — arm and expire at once — and must not be
    /// mistaken for the absent field, which is why the default is a serde
    /// default rather than an `unwrap_or`.
    #[test]
    fn zero_is_not_the_same_as_absent() {
        assert_eq!(parse(r#"{"minutes":0}"#).minutes(), 0);
    }

    #[test]
    fn a_body_that_is_not_json_is_refused() {
        assert_eq!(ArmInspect::accept(&Bytes::from_static(b"15")).unwrap_err().status, 400);
    }
}
