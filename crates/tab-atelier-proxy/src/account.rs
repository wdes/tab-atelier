// SPDX-License-Identifier: MPL-2.0

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
//! # Why JSONL, and why one file per day
//!
//! Samples are appended one JSON object per line, in the same shape as
//! `.claude/scripts/claude-usage-monitor.mjs` writes — deliberately, so
//! anything already reading those files reads these too.
//!
//! JSONL rather than a JSON document because appending is the only write that
//! ever happens. A document would have to be read, parsed, re-serialised and
//! rewritten for every sample: O(n) work that grows all year, a lock to stop
//! two writers interleaving, and a crash mid-write that loses THE WHOLE
//! HISTORY rather than one line. An append is a single `write()`, and a
//! truncated tail costs exactly the sample being written.
//!
//! One file per UTC day (`YYYY-MM-DD_account-usage.jsonl`) because a single
//! growing file has no natural end. Dated files make retention a matter of
//! deleting old ones instead of rewriting a survivor, and they sort
//! lexicographically into chronological order, so "the last week" is a
//! directory listing rather than a scan.

use std::io::Write as _;
use std::path::{Path, PathBuf};

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

    /// Every ratio as a fraction of one.
    ///
    /// Applied at the boundary, to every sample entering the store, so that
    /// NOTHING downstream has to ask which convention it is holding. Getting
    /// this wrong in one place is not a rounding error: the endpoint reports
    /// `35` for 35%, and a consumer that multiplies by 100 renders "3500%" and
    /// pins a chart to its ceiling — which is exactly what shipped.
    #[must_use]
    pub fn normalised(mut self) -> Self {
        self.five_hour = self.five_hour.map(as_fraction);
        self.seven_day = self.seven_day.map(as_fraction);
        self.seven_day_sonnet = self.seven_day_sonnet.map(as_fraction);
        self
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
    s.normalised()
}

/// Utilisation is reported either as a fraction or as a percentage depending
/// on the field; normalise so callers never have to wonder.
#[must_use]
pub fn as_fraction(v: f64) -> f64 {
    if v > 1.0 { (v / 100.0).min(1.0) } else { v.max(0.0) }
}

/// The suffix every daily log carries, after its `YYYY-MM-DD_` prefix.
pub const LOG_SUFFIX: &str = "_account-usage.jsonl";

/// Days of daily logs to keep. Matches the usage buckets' retention, so the
/// two halves of the history end at the same place rather than one outliving
/// the other by months.
pub const RETAIN_DAYS: usize = 90;

/// The rolling log of samples.
#[derive(Debug)]
pub struct Monitor {
    dir: PathBuf,
    recent: Vec<Sample>,
}

/// How many samples to keep in memory for the dashboard. At one every five
/// minutes this is a bit over two days, which is as far back as anyone looks
/// when asking "why is it throttling right now".
const KEEP: usize = 600;

/// The UTC day a sample belongs to, from its own timestamp.
///
/// Taken from the sample rather than the clock, so a sample written a moment
/// after midnight still lands in the day it describes.
fn day_of(sample: &Sample) -> String {
    let day = sample.ts.get(..10).unwrap_or_default();
    if day.len() == 10 && day.as_bytes()[4] == b'-' {
        day.to_owned()
    } else {
        // An unparseable timestamp still has to go somewhere findable.
        "0000-00-00".to_owned()
    }
}

impl Monitor {
    /// Load the tail of the daily logs in `dir`, or start empty.
    #[must_use]
    pub fn load(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let mut files = Self::logs_in(&dir);
        // Newest last, and only enough files to satisfy KEEP.
        files.sort();
        let mut recent: Vec<Sample> = Vec::new();
        for f in files.iter().rev().take(RETAIN_DAYS) {
            let Ok(raw) = std::fs::read_to_string(f) else { continue };
            // Normalised on the way in: logs written before this was done hold
            // percentages, and mixing the two conventions in one series is
            // worse than either alone.
            let mut day: Vec<Sample> = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<Sample>(l).ok().map(Sample::normalised))
                .collect();
            day.append(&mut recent);
            recent = day;
            if recent.len() >= KEEP {
                break;
            }
        }
        if recent.len() > KEEP {
            recent.drain(..recent.len() - KEEP);
        }
        Self { dir, recent }
    }

    /// Every daily log in the directory, including the single undated file
    /// earlier versions wrote — it is still someone's history.
    fn logs_in(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let legacy = dir.join("account-usage.jsonl");
        if legacy.is_file() {
            out.push(legacy);
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(LOG_SUFFIX) && name.len() > LOG_SUFFIX.len() {
                    out.push(e.path());
                }
            }
        }
        out
    }

    /// Append a sample to its day's log.
    pub fn push(&mut self, sample: Sample) {
        let sample = sample.normalised();
        let path = self.dir.join(format!("{}{LOG_SUFFIX}", day_of(&sample)));
        if let Ok(line) = serde_json::to_string(&sample) {
            let _ = std::fs::create_dir_all(&self.dir);
            // Append, never rewrite: a partial line loses one sample, where a
            // truncated rewrite would lose the history.
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.recent.push(sample);
        if self.recent.len() > KEEP {
            self.recent.drain(..self.recent.len() - KEEP);
        }
        self.prune();
    }

    /// Delete logs past the retention window.
    ///
    /// Deleting whole days is the point of dating the files: the alternative,
    /// trimming lines out of one long file, means rewriting it — which is the
    /// operation JSONL exists to avoid.
    fn prune(&self) {
        let mut files = Self::logs_in(&self.dir);
        if files.len() <= RETAIN_DAYS {
            return;
        }
        files.sort();
        let over = files.len() - RETAIN_DAYS;
        for old in files.into_iter().take(over) {
            let _ = std::fs::remove_file(old);
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
        self.utilization_at(crate::usage::now_secs())
    }

    /// Utilisation as of `now`, or `None` if the newest good reading is too
    /// old to act on.
    ///
    /// The staleness bound is the point of this function. `latest()` skips
    /// failed samples, so without it a monitor that broke at noon keeps
    /// answering with noon's number indefinitely — and this value feeds
    /// routing, which would go on treating a plan it cannot see as the 11% it
    /// last measured. An unknown plan and an idle plan are not the same
    /// claim; only one of them is honest here.
    #[must_use]
    pub fn utilization_at(&self, now: u64) -> Option<f64> {
        let latest = self.latest()?;
        let age = epoch_of(&latest.ts).map(|t| now.saturating_sub(t));
        // An unparseable timestamp is not evidence of freshness.
        if age? > STALE_AFTER_SECS {
            return None;
        }
        // Already a fraction: everything entering the store is normalised.
        latest.utilization()
    }

    /// Whether the monitor itself is working, for the dashboard.
    ///
    /// Separate from utilisation because "the plan is at 11%" and "we last
    /// heard that ten hours ago" are different facts, and a chart that only
    /// plots the first silently drew the second as a line that stops.
    #[must_use]
    pub fn health(&self, now: u64) -> Health {
        let last_ok = self.latest();
        let newest = self.recent.last();
        Health {
            stale: self.utilization_at(now).is_none() && last_ok.is_some(),
            last_ok_ts: last_ok.map(|s| s.ts.clone()),
            last_ok_age_secs: last_ok.and_then(|s| epoch_of(&s.ts)).map(|t| now.saturating_sub(t)),
            // The newest sample's error, not the newest error: if the monitor
            // has recovered, there is nothing to report.
            last_error: newest.and_then(|s| s.error.clone()),
            consecutive_failures: u32::try_from(self.recent.iter().rev().take_while(|s| !s.is_ok()).count())
                .unwrap_or(u32::MAX),
        }
    }
}

/// How old the newest good reading may be before it stops counting.
///
/// Twenty minutes is four missed polls at the healthy five-minute interval —
/// long enough that one slow or rate-limited call changes nothing, short
/// enough that a monitor which has genuinely stopped is not still steering
/// routing an hour later.
pub const STALE_AFTER_SECS: u64 = 20 * 60;

/// What the dashboard needs to say about the monitor rather than the plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    /// A reading exists but is too old to act on.
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

/// Seconds since the epoch for an RFC3339 timestamp.
///
/// Parsing is jiff's, not ours: this used to be a hand-rolled days-from-civil
/// conversion, which accepted `2026-02-30` (silently March 2nd) and hour 99
/// because it only range-checked the month and the day. jiff is already
/// compiled into this binary — `env_logger` pulls it in for `humantime` — so
/// the correct parser costs nothing the calendar arithmetic was not already
/// costing.
///
/// A timestamp before 1970 yields `None`; every producer of these files writes
/// `now()`, so that is a malformed file rather than a date to reason about.
#[must_use]
pub fn epoch_of(ts: &str) -> Option<u64> {
    u64::try_from(ts.parse::<jiff::Timestamp>().ok()?.as_second()).ok()
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
        p
    }

    /// A minute after `ts` — the "now" to judge a fixture sample against.
    ///
    /// Fixtures carry fixed dates, so asserting on `utilization()` would tie
    /// the test to the wall clock and start failing twenty minutes after the
    /// date someone wrote in the string.
    fn just_after(ts: &str) -> u64 {
        epoch_of(ts).map_or(0, |t| t + 60)
    }

    fn sample_on(day: &str, five_hour: f64) -> Sample {
        Sample {
            ts: format!("{day}T12:00:00Z"),
            http: Some(200),
            five_hour: Some(five_hour),
            ..Sample::default()
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
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
    /// The bug this exists to prevent, from a live dashboard: the endpoint
    /// reports 35 for 35%, the UI multiplied by 100, and the card read
    /// "7-day: 3500%" with the chart pinned to its ceiling.
    #[test]
    fn a_percentage_from_upstream_becomes_a_fraction_everywhere() {
        let body = r#"{"five_hour":{"utilization":16},"seven_day":{"utilization":35},
                       "seven_day_sonnet":{"utilization":4}}"#;
        let s = parse_usage(200, body, "t".to_owned());
        assert_eq!(s.five_hour, Some(0.16));
        assert_eq!(s.seven_day, Some(0.35), "35 means 35%, not 3500%");
        assert_eq!(s.seven_day_sonnet, Some(0.04));

        // A log written before normalisation held raw percentages; reading it
        // must not produce a series that mixes the two conventions.
        let dir = tmp("legacy-percent");
        std::fs::write(
            dir.join("2026-09-09_account-usage.jsonl"),
            "{\"ts\":\"2026-09-09T08:00:00Z\",\"http\":200,\"five_hour\":37.0,\"seven_day\":19.0}\n",
        )
        .expect("write");
        let m = Monitor::load(&dir);
        assert_eq!(m.utilization_at(just_after("2026-09-09T08:00:00Z")), Some(0.37));
        assert_eq!(
            m.recent()[0].seven_day,
            Some(0.19),
            "the whole sample is normalised, not just the headline"
        );
    }

    #[test]
    fn utilisation_normalises_to_a_fraction() {
        assert!((as_fraction(0.62) - 0.62).abs() < 1e-9);
        assert!((as_fraction(62.0) - 0.62).abs() < 1e-9);
        assert!((as_fraction(-3.0) - 0.0).abs() < 1e-9);
        assert!((as_fraction(400.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn samples_append_and_survive_a_reload() {
        let dir = tmp("append");
        let mut m = Monitor::load(&dir);
        m.push(parse_usage(200, BODY, "2026-09-08T09:00:00Z".to_owned()));
        m.push(parse_usage(500, "{}", "2026-09-08T09:05:00Z".to_owned()));
        assert_eq!(m.recent().len(), 2);
        // latest() skips the failed reading: a 500 is not evidence the plan is
        // idle, and treating it as 0% would release the brakes exactly when
        // something is wrong.
        assert_eq!(
            m.latest().map(|s| s.ts.clone()),
            Some("2026-09-08T09:00:00Z".to_owned())
        );

        let reloaded = Monitor::load(&dir);
        assert_eq!(reloaded.recent().len(), 2, "the JSONL tail must survive a restart");
        let util = reloaded.utilization_at(just_after("2026-09-08T09:00:00Z"));
        assert!((util.unwrap_or(0.0) - 0.62).abs() < 1e-9);
    }

    /// One file per UTC day, named so a directory listing sorts into
    /// chronological order.
    #[test]
    fn each_day_gets_its_own_log() {
        let dir = tmp("daily");
        let mut m = Monitor::load(&dir);
        m.push(sample_on("2026-09-07", 0.10));
        m.push(sample_on("2026-09-08", 0.20));
        m.push(sample_on("2026-09-08", 0.30));

        assert_eq!(
            names_in(&dir),
            vec![
                "2026-09-07_account-usage.jsonl".to_owned(),
                "2026-09-08_account-usage.jsonl".to_owned()
            ]
        );
        // The day a sample DESCRIBES, not the day it was written: a sample
        // written just after midnight still belongs to the day it measured.
        let d8 = std::fs::read_to_string(dir.join("2026-09-08_account-usage.jsonl")).expect("read");
        assert_eq!(d8.lines().count(), 2);

        // Reload walks the files newest-last, so the series stays in order.
        let back = Monitor::load(&dir);
        let utils: Vec<_> = back.recent().iter().filter_map(Sample::utilization).collect();
        assert_eq!(utils, vec![0.10, 0.20, 0.30]);
    }

    /// Retention is deleting whole days — the operation dated files exist to
    /// make possible. Trimming lines out of one long file would mean
    /// rewriting it, which is what JSONL avoids.
    #[test]
    fn logs_past_the_retention_window_are_deleted() {
        let dir = tmp("prune");
        let mut m = Monitor::load(&dir);
        // Two full windows' worth of days, oldest first.
        for i in 0..(RETAIN_DAYS + 10) {
            m.push(sample_on(&format!("2020-{:02}-{:02}", 1 + i / 28, 1 + i % 28), 0.5));
        }
        let left = names_in(&dir);
        assert!(
            left.len() <= RETAIN_DAYS,
            "kept {} logs, retention is {RETAIN_DAYS}",
            left.len()
        );
        // The ones kept are the NEWEST, which is the half anybody looks at.
        assert!(left.last().is_some_and(|n| n.starts_with("2020-04")), "{left:?}");
    }

    /// A single undated file is what earlier versions wrote. It is still
    /// somebody's history, so it keeps being read even though nothing appends
    /// to it any more.
    #[test]
    fn the_old_undated_log_is_still_read() {
        let dir = tmp("legacy");
        std::fs::write(
            dir.join("account-usage.jsonl"),
            serde_json::to_string(&sample_on("2026-01-01", 0.42)).expect("encode") + "\n",
        )
        .expect("write legacy");

        let mut m = Monitor::load(&dir);
        assert_eq!(m.recent().len(), 1, "the undated log must not be ignored");
        // New samples go to dated files; the old one is left alone.
        m.push(sample_on("2026-09-08", 0.50));
        assert!(dir.join("2026-09-08_account-usage.jsonl").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join("account-usage.jsonl"))
                .expect("read")
                .lines()
                .count(),
            1,
            "nothing new should be appended to the undated file"
        );
    }

    #[test]
    fn no_samples_means_no_reading_not_zero() {
        let m = Monitor::load(tmp("empty"));
        assert_eq!(m.utilization(), None, "unknown must not read as 'plenty left'");
    }

    /// The timestamp in a sample, `secs` seconds after the epoch, in the
    /// format the monitor writes.
    fn ts_at(secs: u64) -> String {
        i64::try_from(secs)
            .ok()
            .and_then(|s| jiff::Timestamp::from_second(s).ok())
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    #[test]
    fn a_timestamp_survives_the_round_trip_it_was_written_with() {
        // Held against the same civil-from-days the binary formats with, so
        // the two cannot drift apart: a parser that is wrong by a day would
        // mark every reading stale and silently blind routing.
        for secs in [0_u64, 1_000_000, 1_788_000_000, 1_788_991_236, 4_102_444_800] {
            let ts = ts_at(secs);
            assert_eq!(epoch_of(&ts), Some(secs), "round trip {ts}");
        }
    }

    #[test]
    fn the_timestamp_spellings_the_monitors_actually_write_all_parse() {
        // The Rust monitor writes `…Z`; the .mjs one writes fractional
        // seconds with a `+00:00` offset. Both files feed the same store.
        let base = epoch_of("2026-09-09T12:20:36Z").expect("plain Z");
        assert_eq!(epoch_of("2026-09-09T12:20:36.562143+00:00"), Some(base));
        assert_eq!(epoch_of("2026-09-09T12:20:36.5Z"), Some(base));
        assert_eq!(epoch_of("2026-09-09t12:20:36z"), Some(base));
        // A real offset is converted, not refused: the hand-rolled parser
        // this replaced could not do the arithmetic, so it rejected them.
        assert_eq!(epoch_of("2026-09-09T12:20:36+02:00"), Some(base - 2 * 3600));
        // Impossible dates and times are rejected rather than normalised
        // into some other day — the old parser accepted Feb 30 as March 2.
        assert_eq!(epoch_of("2026-02-30T12:00:00Z"), None, "Feb 30");
        assert_eq!(epoch_of("2026-09-09T99:20:36Z"), None, "hour 99");
        assert_eq!(epoch_of("2026-09-09T12:99:36Z"), None, "minute 99");
        assert_eq!(epoch_of("not a timestamp"), None);
        assert_eq!(epoch_of("2026-13-09T12:20:36Z"), None, "month 13");
        assert_eq!(epoch_of(""), None);
    }

    #[test]
    fn a_reading_stops_counting_once_it_is_too_old() {
        // The production failure this exists for: the last good sample was
        // 12:20:36, every poll after it failed, and the stale number went on
        // steering routing for ten hours.
        let mut m = Monitor::load(tmp("stale"));
        let t0 = 1_788_000_000;
        m.push(Sample {
            ts: ts_at(t0),
            http: Some(200),
            five_hour: Some(0.11),
            ..Sample::default()
        });
        assert_eq!(m.utilization_at(t0 + 60), Some(0.11), "fresh reading counts");
        assert_eq!(
            m.utilization_at(t0 + STALE_AFTER_SECS - 1),
            Some(0.11),
            "inside the window"
        );
        assert_eq!(m.utilization_at(t0 + STALE_AFTER_SECS + 1), None, "outside it, unknown");
    }

    #[test]
    fn the_dashboard_reads_the_field_names_health_actually_serialises() {
        // app.js renders this block; a rename on either side turns the
        // warning banner back off silently, which is precisely the failure
        // the banner exists to prevent.
        let json = serde_json::to_value(Health {
            stale: true,
            last_ok_ts: Some("2026-09-09T12:20:36Z".to_owned()),
            last_ok_age_secs: Some(36_060),
            last_error: Some("http 429".to_owned()),
            consecutive_failures: 2,
        })
        .expect("Health serialises");
        // Either half of the UI may do the reading: computed properties live
        // in app.js, the markup that shows them in index.html.
        let ui = concat!(include_str!("../assets/app.js"), include_str!("../assets/index.html"));
        for field in ["stale", "consecutive_failures", "last_ok_ts", "last_error"] {
            assert!(json.get(field).is_some(), "Health has no `{field}`");
            assert!(
                ui.contains(field),
                "the dashboard never reads `{field}` — it cannot show what it is not told"
            );
        }
    }

    #[test]
    fn health_reports_the_failure_the_chart_cannot_show() {
        let mut m = Monitor::load(tmp("health"));
        let t0 = 1_788_000_000;
        m.push(Sample {
            ts: ts_at(t0),
            http: Some(200),
            five_hour: Some(0.11),
            ..Sample::default()
        });
        let ok = m.health(t0 + 60);
        assert!(!ok.stale);
        assert_eq!(ok.consecutive_failures, 0);
        assert_eq!(ok.last_error, None);

        // The two real failures, in the order they happened.
        m.push(Sample {
            ts: ts_at(t0 + 300),
            http: Some(429),
            error: Some("http 429".to_owned()),
            ..Sample::default()
        });
        m.push(Sample {
            ts: ts_at(t0 + 36_000),
            error: Some("refresh rejected: http 400: invalid_grant".to_owned()),
            ..Sample::default()
        });
        let bad = m.health(t0 + 36_060);
        assert!(bad.stale, "a reading exists but is far too old");
        assert_eq!(bad.consecutive_failures, 2);
        assert_eq!(
            bad.last_error.as_deref(),
            Some("refresh rejected: http 400: invalid_grant"),
            "the newest failure, not the first"
        );
        assert_eq!(bad.last_ok_ts.as_deref(), Some(ts_at(t0).as_str()));
        assert_eq!(bad.last_ok_age_secs, Some(36_060));
    }
}
