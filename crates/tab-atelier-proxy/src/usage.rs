// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Who spent what — the question a shared token could never answer.
//!
//! The proxy sees every call, so it is the only place that can attribute token
//! spend to a person without asking anyone to self-report. Anthropic returns
//! the counts it billed; this reads them off the response as it streams past
//! and files them under the account whose key opened the request.
//!
//! # Buckets, not a log
//!
//! Usage is kept as hourly buckets per account, not as one row per request.
//! A busy fleet makes millions of requests and nobody plots them individually;
//! an append-only log would grow without bound, and answering "last 7 days"
//! would mean reading all of it. Hourly is the finest granularity any of the
//! charts draw, [`RETAIN_HOURS`] of them is a bounded amount of state, and
//! every question the dashboard asks is a sum over a slice.
//!
//! It is deliberately NOT an audit log: no prompts, no request bodies, no
//! per-call records. Counts and timestamps only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Roughly 90 days. Long enough to see a quarter's shape, short enough that
/// the file stays small even with a large team.
pub const RETAIN_HOURS: u64 = 24 * 90;

const HOUR: u64 = 3600;

/// The four counts Anthropic bills separately.
///
/// Cache reads and cache writes are priced differently from ordinary input, so
/// collapsing them into one "input" number would misreport the thing people
/// actually want to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

impl Tokens {
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    const fn add(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }

    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// One hour of one account's activity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bucket {
    /// Unix seconds, truncated to the hour.
    pub hour: u64,
    pub calls: u64,
    /// Calls the upstream refused (4xx/5xx). Kept apart from `calls` so a
    /// spike of failures cannot read as a spike of usage.
    #[serde(default)]
    pub errors: u64,
    #[serde(flatten)]
    pub tokens: Tokens,
}

/// One account's history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountUsage {
    /// Ascending by hour, one entry per hour that had traffic.
    #[serde(default)]
    pub buckets: Vec<Bucket>,
    /// All-time totals per model. Small, and it answers "what is this person
    /// actually running" without scanning buckets.
    #[serde(default)]
    pub by_model: BTreeMap<String, Tokens>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    accounts: BTreeMap<String, AccountUsage>,
}

/// The usage store, persisted as JSON beside the accounts.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    accounts: BTreeMap<String, AccountUsage>,
    /// Last time this was written. Recording happens on every proxied request,
    /// and rewriting the file each time would make the disk the bottleneck on
    /// a busy proxy — so writes are coalesced.
    last_save: u64,
    dirty: bool,
}

/// How long a change may sit unwritten. A crash loses at most this much
/// accounting, which is the right trade against an fsync per API call.
const SAVE_EVERY_SECS: u64 = 30;

#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[must_use]
pub const fn hour_of(ts: u64) -> u64 {
    ts - (ts % HOUR)
}

impl Store {
    /// Load, or start empty.
    ///
    /// Unlike the account store, a malformed usage file is NOT fatal: usage is
    /// accounting, not access control. Losing it must never stop the proxy
    /// from serving — the file is renamed aside and a fresh one started, so
    /// the damaged copy is still there to look at.
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let accounts = match std::fs::read_to_string(&path) {
            Ok(raw) if !raw.trim().is_empty() => match serde_json::from_str::<Doc>(&raw) {
                Ok(d) => d.accounts,
                Err(e) => {
                    log::warn!("usage: {} is unreadable ({e}); starting fresh", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    BTreeMap::new()
                }
            },
            _ => BTreeMap::new(),
        };
        Self {
            path,
            accounts,
            last_save: 0,
            dirty: false,
        }
    }

    /// File one call against an account.
    pub fn record(&mut self, account_id: &str, model: Option<&str>, tokens: Tokens, ok: bool) {
        let hour = hour_of(now_secs());
        let entry = self.accounts.entry(account_id.to_owned()).or_default();

        // Buckets are ascending, and traffic arrives in time order, so the one
        // being written to is almost always the last.
        let bucket = match entry.buckets.last_mut() {
            Some(b) if b.hour == hour => b,
            _ => {
                entry.buckets.push(Bucket {
                    hour,
                    ..Bucket::default()
                });
                entry.buckets.last_mut().unwrap_or_else(|| unreachable!("just pushed"))
            }
        };
        bucket.calls += 1;
        if !ok {
            bucket.errors += 1;
        }
        bucket.tokens.add(tokens);

        if let Some(m) = model.filter(|_| !tokens.is_zero()) {
            entry.by_model.entry(m.to_owned()).or_default().add(tokens);
        }

        let cutoff = hour.saturating_sub(RETAIN_HOURS * HOUR);
        entry.buckets.retain(|b| b.hour >= cutoff);

        self.dirty = true;
        self.maybe_save();
    }

    fn maybe_save(&mut self) {
        let now = now_secs();
        if self.dirty && now.saturating_sub(self.last_save) >= SAVE_EVERY_SECS {
            let _ = self.save();
        }
    }

    /// Write now, whatever the coalescing window says.
    ///
    /// # Errors
    /// Anything that stops the file reaching disk.
    pub fn save(&mut self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string(&Doc {
            accounts: self.accounts.clone(),
        })
        .map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename into {}: {e}", self.path.display()))?;
        self.last_save = now_secs();
        self.dirty = false;
        Ok(())
    }

    #[must_use]
    pub fn for_account(&self, id: &str) -> Option<&AccountUsage> {
        self.accounts.get(id)
    }

    /// Sum a window ending now. `hours` of 0 means everything retained.
    #[must_use]
    pub fn totals(&self, id: &str, hours: u64) -> (u64, u64, Tokens) {
        let cutoff = if hours == 0 {
            0
        } else {
            hour_of(now_secs()).saturating_sub((hours - 1) * HOUR)
        };
        let mut calls = 0;
        let mut errors = 0;
        let mut tokens = Tokens::default();
        if let Some(a) = self.accounts.get(id) {
            for b in a.buckets.iter().filter(|b| b.hour >= cutoff) {
                calls += b.calls;
                errors += b.errors;
                tokens.add(b.tokens);
            }
        }
        (calls, errors, tokens)
    }

    /// A dense hourly series ending at the current hour.
    ///
    /// Dense matters: hours with no traffic must appear as zeroes, or a chart
    /// drawn from this silently compresses idle time and every gap reads as
    /// activity that never happened.
    #[must_use]
    pub fn series(&self, id: &str, hours: u64) -> Vec<Bucket> {
        let end = hour_of(now_secs());
        let start = end.saturating_sub(hours.saturating_sub(1) * HOUR);
        let mut filled: Vec<Bucket> = (0..hours)
            .map(|i| Bucket {
                hour: start + i * HOUR,
                ..Bucket::default()
            })
            .collect();
        if let Some(a) = self.accounts.get(id) {
            for b in &a.buckets {
                if b.hour >= start && b.hour <= end {
                    let idx = ((b.hour - start) / HOUR) as usize;
                    if let Some(slot) = filled.get_mut(idx) {
                        slot.calls = b.calls;
                        slot.errors = b.errors;
                        slot.tokens = b.tokens;
                    }
                }
            }
        }
        filled
    }

    /// Every account that has ever been recorded.
    #[must_use]
    pub fn account_ids(&self) -> Vec<String> {
        self.accounts.keys().cloned().collect()
    }

    /// Drop an account's history — called when the account is deleted, so
    /// "forget this person" actually forgets them.
    pub fn forget(&mut self, id: &str) {
        if self.accounts.remove(id).is_some() {
            self.dirty = true;
            let _ = self.save();
        }
    }
}

/// Reads token counts off a response as it streams past.
///
/// Anthropic reports usage in two shapes and this has to handle both:
///
/// * a plain JSON reply carries one `usage` object;
/// * an SSE stream reports input counts in `message_start` and then the
///   running output count in each `message_delta` — the LAST of which is the
///   real total, so later values replace earlier ones rather than adding.
///
/// Memory is bounded either way. The SSE path processes whole lines and drops
/// them, so a generation of any length costs one partial line; the JSON path
/// accumulates to a cap and gives up past it rather than buffering a large
/// response just to count it.
#[derive(Debug)]
pub struct Sniffer {
    sse: bool,
    pending: Vec<u8>,
    tokens: Tokens,
    model: Option<String>,
    over_cap: bool,
}

/// Enough for any `messages` reply; past this we stop trying rather than hold
/// a big body in memory for accounting.
const JSON_CAP: usize = 256 * 1024;

impl Sniffer {
    #[must_use]
    pub fn new(content_type: Option<&str>) -> Self {
        Self {
            sse: content_type.is_some_and(|c| c.contains("event-stream")),
            pending: Vec::new(),
            tokens: Tokens::default(),
            model: None,
            over_cap: false,
        }
    }

    /// Feed the next chunk on its way to the client.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.over_cap {
            return;
        }
        self.pending.extend_from_slice(chunk);
        if !self.sse {
            if self.pending.len() > JSON_CAP {
                self.over_cap = true;
                self.pending = Vec::new();
            }
            return;
        }
        // Line-oriented: take each complete line, use it, drop it.
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=nl).collect();
            self.take_line(&line);
        }
        // A single line this long is not an SSE frame we understand.
        if self.pending.len() > JSON_CAP {
            self.over_cap = true;
            self.pending = Vec::new();
        }
    }

    fn take_line(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else { return };
        let Some(payload) = text.trim_end().strip_prefix("data:") else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
            return;
        };
        self.take_value(&v);
    }

    fn take_value(&mut self, value: &serde_json::Value) {
        // message_start nests the model and the input counts under `message`;
        // message_delta and a non-streamed reply carry `usage` at the top.
        if let Some(name) = value
            .get("message")
            .and_then(|msg| msg.get("model"))
            .or_else(|| value.get("model"))
            .and_then(serde_json::Value::as_str)
        {
            self.model = Some(name.to_owned());
        }
        let found = value
            .get("message")
            .and_then(|msg| msg.get("usage"))
            .or_else(|| value.get("usage"));
        let Some(counts) = found else { return };
        let n = |key: &str| counts.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
        // Replace rather than accumulate: `message_delta` repeats a RUNNING
        // total, so adding them would multiply a long generation's output
        // count by the number of deltas.
        let (i, o, cr, cw) = (
            n("input_tokens"),
            n("output_tokens"),
            n("cache_read_input_tokens"),
            n("cache_creation_input_tokens"),
        );
        if i > 0 {
            self.tokens.input = i;
        }
        if o > 0 {
            self.tokens.output = o;
        }
        if cr > 0 {
            self.tokens.cache_read = cr;
        }
        if cw > 0 {
            self.tokens.cache_write = cw;
        }
    }

    /// What the response reported.
    #[must_use]
    pub fn finish(mut self) -> (Option<String>, Tokens) {
        if !self.sse && !self.over_cap && !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&pending) {
                self.take_value(&v);
            }
        }
        (self.model, self.tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ta-proxy-usage-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p.join("usage.json")
    }

    #[test]
    fn a_plain_json_reply_reports_its_usage() {
        let mut s = Sniffer::new(Some("application/json"));
        s.feed(br#"{"model":"claude-opus-5","usage":{"input_tokens":12,"#);
        s.feed(br#""output_tokens":34,"cache_read_input_tokens":5}}"#);
        let (model, t) = s.finish();
        assert_eq!(model.as_deref(), Some("claude-opus-5"));
        assert_eq!(t.input, 12);
        assert_eq!(t.output, 34);
        assert_eq!(t.cache_read, 5);
        assert_eq!(t.total(), 51);
    }

    /// The failure this guards: `message_delta` repeats a RUNNING output total,
    /// so summing the deltas reports several times the tokens actually billed.
    #[test]
    fn a_streamed_reply_takes_the_last_running_total_not_their_sum() {
        let mut s = Sniffer::new(Some("text/event-stream"));
        s.feed(b"event: message_start\n");
        s.feed(br#"data: {"type":"message_start","message":{"model":"claude-opus-5","usage":{"input_tokens":100,"cache_read_input_tokens":7}}}"#);
        s.feed(b"\n\n");
        for running in [10u64, 25, 40] {
            s.feed(
                format!("data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":{running}}}}}\n\n")
                    .as_bytes(),
            );
        }
        s.feed(b"data: [DONE]\n\n");
        let (model, t) = s.finish();
        assert_eq!(model.as_deref(), Some("claude-opus-5"));
        assert_eq!(t.input, 100);
        assert_eq!(t.cache_read, 7);
        assert_eq!(t.output, 40, "the last running total, not 10+25+40");
    }

    /// A chunk boundary can fall anywhere, including mid-token of the JSON.
    #[test]
    fn sse_frames_split_across_chunks_still_parse() {
        let mut s = Sniffer::new(Some("text/event-stream"));
        let frame = br#"data: {"type":"message_delta","usage":{"output_tokens":99}}"#;
        for byte in frame {
            s.feed(&[*byte]);
        }
        s.feed(b"\n");
        assert_eq!(s.finish().1.output, 99);
    }

    #[test]
    fn a_long_stream_does_not_accumulate_memory() {
        let mut s = Sniffer::new(Some("text/event-stream"));
        for _ in 0..2000 {
            s.feed(br#"data: {"type":"content_block_delta","delta":{"text":"................"}}"#);
            s.feed(b"\n");
        }
        assert!(s.pending.len() < 1024, "processed lines must be dropped, not buffered");
    }

    #[test]
    fn usage_is_bucketed_by_hour_and_attributed_per_account() {
        let mut s = Store::load(tmp("bucket"));
        s.record(
            "ada",
            Some("claude-opus-5"),
            Tokens {
                input: 10,
                output: 5,
                ..Tokens::default()
            },
            true,
        );
        s.record(
            "ada",
            Some("claude-opus-5"),
            Tokens {
                input: 3,
                output: 1,
                ..Tokens::default()
            },
            true,
        );
        s.record("grace", Some("claude-haiku-4-5"), Tokens::default(), false);

        let (calls, errors, t) = s.totals("ada", 0);
        assert_eq!((calls, errors), (2, 0));
        assert_eq!((t.input, t.output), (13, 6));
        // The whole point: one person's spend is not another's.
        let (gcalls, gerrors, gt) = s.totals("grace", 0);
        assert_eq!(
            (gcalls, gerrors),
            (1, 1),
            "a refused call counts as a call and an error"
        );
        assert!(gt.is_zero());

        let ada = s.for_account("ada").expect("ada");
        assert_eq!(ada.buckets.len(), 1, "two calls in the same hour share a bucket");
        assert_eq!(ada.by_model.get("claude-opus-5").map(Tokens::total), Some(19));
    }

    #[test]
    fn the_series_is_dense_so_idle_hours_are_visible() {
        let mut s = Store::load(tmp("dense"));
        s.record(
            "ada",
            None,
            Tokens {
                input: 1,
                ..Tokens::default()
            },
            true,
        );
        let series = s.series("ada", 24);
        assert_eq!(series.len(), 24, "a fixed-width window, gaps included");
        assert_eq!(series.last().map(|b| b.calls), Some(1), "now is the last bucket");
        assert!(
            series[..23].iter().all(|b| b.calls == 0),
            "quiet hours must be zeroes, not missing"
        );
        // Ascending, so a chart can plot it without sorting.
        assert!(series.windows(2).all(|w| w[0].hour < w[1].hour));
    }

    #[test]
    fn history_survives_a_reload_and_deletion_forgets_it() {
        let path = tmp("persist");
        let mut s = Store::load(&path);
        s.record(
            "ada",
            Some("m"),
            Tokens {
                input: 7,
                ..Tokens::default()
            },
            true,
        );
        s.save().expect("save");

        let mut reloaded = Store::load(&path);
        assert_eq!(reloaded.totals("ada", 0).2.input, 7);
        reloaded.forget("ada");
        assert!(reloaded.for_account("ada").is_none());
        assert!(Store::load(&path).for_account("ada").is_none(), "and it stays gone");
    }

    /// Accounting must never be able to stop the proxy serving.
    #[test]
    fn a_corrupt_usage_file_is_survivable() {
        let path = tmp("corrupt");
        std::fs::write(&path, "{not json").expect("write");
        let s = Store::load(&path);
        assert!(s.account_ids().is_empty());
        assert!(
            path.with_extension("json.corrupt").exists(),
            "the damaged file is kept for inspection, not deleted"
        );
    }
}
