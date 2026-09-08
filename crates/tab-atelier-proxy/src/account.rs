// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How much of the *subscription* is left — the pressure everyone shares.
//!
//! Per-account token counts say who spent what. They cannot say how close the
//! shared Claude plan is to its limit, because that limit is not denominated
//! in anything the proxy can see from a response body.
//!
//! Anthropic will simply tell us: `GET /api/oauth/usage`, with the same OAuth
//! token the egress already holds, reports utilisation of the rolling 5-hour
//! and 7-day windows. That is the honest pressure signal — not an inference
//! from rate-limit headers, and not a guess from our own accounting.
//!
//! Samples are appended as JSONL, one object per line, in the same shape as
//! `.claude/scripts/claude-usage-monitor.mjs` writes. Deliberately the same
//! format: anything already reading those files reads these too, and a line
//! per sample survives a crash mid-write in a way a rewritten JSON document
//! does not.

use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One reading of the shared plan's utilisation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    /// RFC3339, matching the .mjs monitor's `ts`.
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    /// 0.0–1.0 of the rolling five-hour window. The one that actually bites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_resets: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_sonnet: Option<f64>,
    /// Set when the reading failed. A sample is still appended, so a gap in
    /// the file always means "the monitor was not running", never "the call
    /// failed and we said nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Sample {
    /// The number to act on: the tightest window that is actually reported.
    ///
    /// Five-hour first because it is the one that stops work mid-afternoon;
    /// the seven-day figure moves too slowly to schedule against.
    #[must_use]
    pub fn utilization(&self) -> Option<f64> {
        self.five_hour.or(self.seven_day)
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.error.is_none() && self.http.is_some_and(|c| (200..300).contains(&c))
    }
}

/// Parse the `/api/oauth/usage` body into a sample.
///
/// Tolerant on purpose: a field Anthropic adds, renames or omits must not turn
/// into a hard failure that stops the monitor. A missing number is `None`, and
/// `None` means "do not act on it" everywhere downstream.
#[must_use]
pub fn parse_usage(status: u16, body: &str, ts: String) -> Sample {
    let mut s = Sample {
        ts,
        http: Some(status),
        ..Sample::default()
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        s.error = Some(format!("unparseable body ({} bytes)", body.len()));
        return s;
    };
    let util = |key: &str| {
        v.get(key)
            .and_then(|w| w.get("utilization"))
            .and_then(serde_json::Value::as_f64)
    };
    s.five_hour = util("five_hour");
    s.seven_day = util("seven_day");
    s.seven_day_sonnet = util("seven_day_sonnet");
    s.five_hour_resets = v
        .get("five_hour")
        .and_then(|w| w.get("resets_at"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if !(200..300).contains(&status) {
        s.error = Some(format!("http {status}"));
    }
    s
}

/// Utilisation is reported either as a fraction or as a percentage depending
/// on the field; normalise so callers never have to wonder.
#[must_use]
pub fn as_fraction(v: f64) -> f64 {
    if v > 1.0 { (v / 100.0).min(1.0) } else { v.max(0.0) }
}

/// The rolling log of samples.
#[derive(Debug)]
pub struct Monitor {
    path: PathBuf,
    recent: Vec<Sample>,
}

/// How many samples to keep in memory for the dashboard. At one every five
/// minutes this is a bit over two days, which is as far back as anyone looks
/// when asking "why is it throttling right now".
const KEEP: usize = 600;

impl Monitor {
    /// Load the tail of an existing log, or start empty.
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let recent = std::fs::read_to_string(&path)
            .map(|raw| {
                raw.lines()
                    .rev()
                    .take(KEEP)
                    .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default();
        Self { path, recent }
    }

    /// Append a sample, in memory and on disk.
    pub fn push(&mut self, sample: Sample) {
        if let Ok(line) = serde_json::to_string(&sample) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Append, never rewrite: a partial line loses one sample, where a
            // truncated rewrite would lose the history.
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.recent.push(sample);
        if self.recent.len() > KEEP {
            self.recent.drain(..self.recent.len() - KEEP);
        }
    }

    #[must_use]
    pub fn latest(&self) -> Option<&Sample> {
        self.recent.iter().rev().find(|s| s.is_ok())
    }

    #[must_use]
    pub fn recent(&self) -> &[Sample] {
        &self.recent
    }

    /// Current utilisation, 0.0–1.0, if it is known.
    #[must_use]
    pub fn utilization(&self) -> Option<f64> {
        self.latest().and_then(Sample::utilization).map(as_fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{
        "five_hour": {"utilization": 0.62, "resets_at": "2026-09-08T14:00:00Z"},
        "seven_day": {"utilization": 0.31},
        "seven_day_sonnet": {"utilization": 0.12}
    }"#;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ta-proxy-acct-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p.join("account-usage.jsonl")
    }

    #[test]
    fn the_oauth_usage_body_parses_into_a_sample() {
        let s = parse_usage(200, BODY, "2026-09-08T09:00:00Z".to_owned());
        assert!(s.is_ok());
        assert_eq!(s.five_hour, Some(0.62));
        assert_eq!(s.seven_day, Some(0.31));
        assert_eq!(s.seven_day_sonnet, Some(0.12));
        assert_eq!(s.five_hour_resets.as_deref(), Some("2026-09-08T14:00:00Z"));
        // Five-hour is the window that bites, so it is the one acted on.
        assert_eq!(s.utilization(), Some(0.62));
    }

    /// A shape change upstream must degrade to "unknown", not to a crash or a
    /// confident wrong number.
    #[test]
    fn an_unexpected_shape_reads_as_unknown_rather_than_failing() {
        for body in ["{}", r#"{"five_hour": {}}"#, r#"{"five_hour": 7}"#, "null"] {
            let s = parse_usage(200, body, "t".to_owned());
            assert_eq!(s.utilization(), None, "body {body} should give no reading");
        }
        let bad = parse_usage(200, "<html>nope", "t".to_owned());
        assert!(bad.error.is_some(), "an unparseable body must be recorded as an error");
        assert!(!bad.is_ok());
    }

    #[test]
    fn a_failed_call_is_still_recorded() {
        let s = parse_usage(401, "{}", "t".to_owned());
        assert!(!s.is_ok());
        assert_eq!(s.error.as_deref(), Some("http 401"));
    }

    /// Percentages and fractions both appear in the wild; downstream code
    /// should never have to ask which it got.
    #[test]
    fn utilisation_normalises_to_a_fraction() {
        assert!((as_fraction(0.62) - 0.62).abs() < 1e-9);
        assert!((as_fraction(62.0) - 0.62).abs() < 1e-9);
        assert!((as_fraction(-3.0) - 0.0).abs() < 1e-9);
        assert!((as_fraction(400.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn samples_append_and_survive_a_reload() {
        let path = tmp("append");
        let mut m = Monitor::load(&path);
        m.push(parse_usage(200, BODY, "t1".to_owned()));
        m.push(parse_usage(500, "{}", "t2".to_owned()));
        assert_eq!(m.recent().len(), 2);
        // latest() skips the failed reading: a 500 is not evidence the plan is
        // idle, and treating it as 0% would release the brakes exactly when
        // something is wrong.
        assert_eq!(m.latest().map(|s| s.ts.clone()), Some("t1".to_owned()));

        let reloaded = Monitor::load(&path);
        assert_eq!(reloaded.recent().len(), 2, "the JSONL tail must survive a restart");
        assert!((reloaded.utilization().unwrap_or(0.0) - 0.62).abs() < 1e-9);
    }

    #[test]
    fn no_samples_means_no_reading_not_zero() {
        let m = Monitor::load(tmp("empty"));
        assert_eq!(m.utilization(), None, "unknown must not read as 'plenty left'");
    }
}
