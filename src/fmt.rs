// SPDX-License-Identifier: MPL-2.0

//! Human-readable renderings shared by the GUI and the CLI.
//!
//! Ungated on purpose. This existed twice — `app::format_duration` and
//! `share_link::fmt_uptime`, the same three branches with the same format
//! strings — because the CLI copy was written on the reasoning that the GUI's
//! lived "behind the `gui` feature". That is true of `app`, and irrelevant: a
//! helper does not have to live in the module that first wanted it. Here it
//! is compiled in both configurations and the two callers agree by
//! construction rather than by having been written the same day.
//!
//! Deliberately not a date library. `chrono` formats instants, not humanised
//! elapsed spans, and `humantime` is not a dependency — pulling one in to
//! delete nine lines would be a poor trade.

use std::time::Duration;

/// `45s` / `2m 5s` / `1h 12m`.
///
/// Coarsens as it grows: seconds matter when something just started and are
/// noise once it has been running for an hour.
#[must_use]
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The same, from a plain second count.
#[must_use]
pub fn duration_secs(secs: u64) -> String {
    duration(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds() {
        assert_eq!(duration(Duration::from_secs(0)), "0s");
        assert_eq!(duration(Duration::from_secs(45)), "45s");
        assert_eq!(duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(duration(Duration::from_mins(1)), "1m 0s");
        assert_eq!(duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(duration(Duration::from_hours(1)), "1h 0m");
        assert_eq!(duration(Duration::from_mins(121)), "2h 1m");
        assert_eq!(duration(Duration::from_hours(24)), "24h 0m");
    }

    #[test]
    fn the_second_count_form_agrees_with_the_duration_form() {
        // The two callers reach this through different doors; they must not
        // be able to disagree at a boundary.
        for secs in [0_u64, 59, 60, 3599, 3600, 86_399, 90_061] {
            assert_eq!(duration_secs(secs), duration(Duration::from_secs(secs)), "at {secs}s");
        }
    }
}
