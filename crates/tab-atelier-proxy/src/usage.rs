// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

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
//!
//! # On disk
//!
//! `usage/<account-id>/YYYY-MM-DD_usage.json` — a directory per account, a
//! file per UTC day. Every hour records WHICH MODEL was billed, because a
//! token total cannot say whether it was Opus or Haiku and the price differs
//! by an order of magnitude. That matters more here than in most proxies:
//! [`crate::fallback`] rewrites the model under pressure, so what the caller
//! asked for and what the plan paid for are not always the same thing.

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
    /// The same hour split by the model that was actually BILLED.
    ///
    /// A total says how much was spent; it cannot say whether that was Opus or
    /// Haiku, which is most of what the number means — the price differs by an
    /// order of magnitude. It matters more here than in most proxies because
    /// [`crate::fallback`] rewrites the model under pressure, so what the
    /// caller asked for and what the plan paid for are not always the same
    /// thing. This records what upstream reported, which is the one that was
    /// charged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_model: BTreeMap<String, Tokens>,
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

/// Where a day of one account's buckets lives, under the usage root.
///
/// `usage/<account-id>/YYYY-MM-DD_usage.json` — a directory per account and a
/// file per UTC day. One file for everybody meant every account's history was
/// rewritten whenever any of them made a call, and it grew without a natural
/// place to stop. Split this way, a write touches one account's current day,
/// retention is deleting old files, and "what did this person spend last
/// Tuesday" is a path rather than a scan.
///
/// An account id is a UUID we generated, so it is safe as a directory name —
/// but it is checked anyway ([`safe_id`]) rather than trusted, because a
/// path built from a stored value is exactly where traversal creeps in.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
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

/// The day part of a bucket's path, from the bucket's own hour.
#[must_use]
pub fn day_of(hour: u64) -> String {
    let days = hour / 86_400;
    let shifted = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_pos = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_pos + 2) / 5 + 1;
    let month = if month_pos < 10 { month_pos + 3 } else { month_pos - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Account ids are UUIDs we minted, but this builds a filesystem path from a
/// stored value, so it is verified rather than trusted.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

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
    /// Load every account's daily files under `root`.
    ///
    /// A malformed file is NOT fatal, unlike the account store: usage is
    /// accounting, not access control, and losing it must never stop the proxy
    /// serving. The damaged file is renamed aside so it is still there to look
    /// at, and the rest of the history loads around it.
    #[must_use]
    pub fn load(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut accounts: BTreeMap<String, AccountUsage> = BTreeMap::new();

        // The single all-accounts file earlier versions wrote. Still someone's
        // history, so it is folded in; nothing writes to it again.
        let legacy = root.join("usage.json");
        if let Ok(raw) = std::fs::read_to_string(&legacy) {
            match serde_json::from_str::<Doc>(&raw) {
                Ok(d) => accounts = d.accounts,
                Err(e) => {
                    log::warn!("usage: {} is unreadable ({e}); ignoring it", legacy.display());
                    let _ = std::fs::rename(&legacy, legacy.with_extension("json.corrupt"));
                }
            }
        }

        if let Ok(entries) = std::fs::read_dir(&root) {
            for account_dir in entries.flatten().filter(|e| e.path().is_dir()) {
                let id = account_dir.file_name().to_string_lossy().into_owned();
                if !safe_id(&id) {
                    continue;
                }
                let entry = accounts.entry(id).or_default();
                let mut files: Vec<PathBuf> = std::fs::read_dir(account_dir.path())
                    .map(|d| {
                        d.flatten()
                            .map(|e| e.path())
                            .filter(|p| p.extension().is_some_and(|x| x == "json"))
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort(); // dated names sort into chronological order
                for f in files {
                    let Ok(raw) = std::fs::read_to_string(&f) else { continue };
                    match serde_json::from_str::<AccountUsage>(&raw) {
                        Ok(day) => {
                            entry.buckets.extend(day.buckets);
                            for (model, t) in day.by_model {
                                entry.by_model.entry(model).or_default().add(t);
                            }
                        }
                        Err(e) => {
                            log::warn!("usage: {} is unreadable ({e}); skipping", f.display());
                            let _ = std::fs::rename(&f, f.with_extension("json.corrupt"));
                        }
                    }
                }
                entry.buckets.sort_by_key(|b| b.hour);
            }
        }

        Self {
            root,
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
            bucket.by_model.entry(m.to_owned()).or_default().add(tokens);
        }

        if let Some(m) = model.filter(|_| !tokens.is_zero()) {
            entry.by_model.entry(m.to_owned()).or_default().add(tokens);
        }

        let cutoff = hour.saturating_sub(RETAIN_HOURS * HOUR);
        // Deduplicated to days as we go: an expired day holds 24 buckets and
        // needs one unlink, not 24.
        let stale: std::collections::BTreeSet<String> = entry
            .buckets
            .iter()
            .filter(|b| b.hour < cutoff)
            .map(|b| day_of(b.hour))
            .collect();
        entry.buckets.retain(|b| b.hour >= cutoff);
        // Days that fell out of the window lose their file, so retention is a
        // deletion rather than a file that shrinks to `[]` and stays forever.
        if safe_id(account_id) {
            for day in stale {
                let _ = std::fs::remove_file(self.root.join(account_id).join(format!("{day}_usage.json")));
            }
        }

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
    /// Only the days that actually hold buckets are written, so a quiet
    /// account costs nothing and a busy one rewrites one small file rather
    /// than everybody's history.
    ///
    /// # Errors
    /// Anything that stops a file reaching disk.
    pub fn save(&mut self) -> Result<(), String> {
        for (id, usage) in &self.accounts {
            if !safe_id(id) {
                continue;
            }
            let dir = self.root.join(id);
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

            // Split this account's buckets by day, then write each day whole.
            let mut days: BTreeMap<String, AccountUsage> = BTreeMap::new();
            for b in &usage.buckets {
                days.entry(day_of(b.hour)).or_default().buckets.push(b.clone());
            }
            for (day, mut doc) in days {
                // The all-time per-model totals belong to the account, not to
                // a day; recording the day's own split keeps each file
                // self-describing when read on its own.
                for b in &doc.buckets {
                    for (model, t) in &b.by_model {
                        doc.by_model.entry(model.clone()).or_default().add(*t);
                    }
                }
                let path = dir.join(format!("{day}_usage.json"));
                let json = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
                }
                std::fs::rename(&tmp, &path).map_err(|e| format!("rename into {}: {e}", path.display()))?;
            }
        }
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
                        slot.by_model.clone_from(&b.by_model);
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
            // The directory goes too, or "forget this person" would leave
            // their history on disk under an id nothing refers to any more.
            if safe_id(id) {
                let _ = std::fs::remove_dir_all(self.root.join(id));
            }
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
        p
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

    /// Which model was billed, hour by hour — not just in the all-time total.
    ///
    /// Model fallback means the model asked for and the model charged can
    /// differ, so "how much did we spend" is only half an answer without it.
    #[test]
    fn each_hour_records_which_model_was_billed() {
        let mut s = Store::load(tmp("models"));
        let opus = Tokens {
            input: 1_000,
            output: 500,
            ..Tokens::default()
        };
        let haiku = Tokens {
            input: 200,
            output: 100,
            ..Tokens::default()
        };
        s.record("ada", Some("claude-opus-5"), opus, true);
        s.record("ada", Some("claude-haiku-4-5"), haiku, true);
        s.record("ada", Some("claude-haiku-4-5"), haiku, true);

        let bucket = &s.for_account("ada").expect("ada").buckets[0];
        assert_eq!(bucket.calls, 3);
        assert_eq!(bucket.by_model.len(), 2, "both models must appear in the hour");
        assert_eq!(bucket.by_model["claude-opus-5"].total(), 1_500);
        assert_eq!(
            bucket.by_model["claude-haiku-4-5"].total(),
            600,
            "two haiku calls summed"
        );
        // The hour's total still agrees with the split.
        let summed: u64 = bucket.by_model.values().map(Tokens::total).sum();
        assert_eq!(summed, bucket.tokens.total());

        // And the series carries it, so a chart can break the hour down.
        let series = s.series("ada", 2);
        let last = series.last().expect("current hour");
        assert_eq!(last.by_model.len(), 2);
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
        assert!(
            !path.join("ada").exists(),
            "the account's directory goes with it — otherwise 'forget' leaves the \
             history on disk under an id nothing refers to any more"
        );
    }

    /// A directory per account, a file per UTC day.
    #[test]
    fn each_account_gets_a_folder_and_each_day_a_file() {
        let root = tmp("layout");
        let mut s = Store::load(&root);
        s.record(
            "ada",
            Some("claude-opus-5"),
            Tokens {
                input: 5,
                ..Tokens::default()
            },
            true,
        );
        s.record(
            "grace",
            Some("claude-haiku-4-5"),
            Tokens {
                input: 3,
                ..Tokens::default()
            },
            true,
        );
        s.save().expect("save");

        let today = day_of(hour_of(now_secs()));
        for who in ["ada", "grace"] {
            let f = root.join(who).join(format!("{today}_usage.json"));
            assert!(f.is_file(), "expected {}", f.display());
        }

        // One account's traffic must not rewrite another's file — that is the
        // point of splitting them.
        let ada = std::fs::read_to_string(root.join("ada").join(format!("{today}_usage.json"))).expect("read");
        assert!(ada.contains("claude-opus-5"));
        assert!(
            !ada.contains("claude-haiku-4-5"),
            "grace's models must not be in ada's file"
        );

        // A day file is self-describing when read on its own.
        let doc: AccountUsage = serde_json::from_str(&ada).expect("parse day");
        assert_eq!(doc.by_model["claude-opus-5"].total(), 5);

        let back = Store::load(&root);
        assert_eq!(back.totals("ada", 0).2.input, 5);
        assert_eq!(back.totals("grace", 0).2.input, 3);
    }

    /// Dates come from the bucket's own hour, and ids never leave the root.
    #[test]
    fn the_day_name_is_the_buckets_utc_date() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_709_164_800), "2024-02-29");
        assert_eq!(day_of(1_757_320_000 - (1_757_320_000 % 3600)), "2025-09-08");
        // The id becomes a directory name, so it is verified rather than
        // trusted — a path built from a stored value is where traversal creeps
        // in, even when we minted the value ourselves.
        assert!(safe_id("6f1e4b2a-0000-4000-8000-000000000000"));
        assert!(!safe_id("../etc"));
        assert!(!safe_id("a/b"));
        assert!(!safe_id(""));
    }

    /// Accounting must never be able to stop the proxy serving.
    #[test]
    fn a_corrupt_usage_file_is_survivable() {
        let root = tmp("corrupt");
        // One damaged day, and one good one, for the same account.
        let dir = root.join("ada");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("2026-09-07_usage.json"), "{not json").expect("write bad");
        std::fs::write(
            dir.join("2026-09-08_usage.json"),
            r#"{"buckets":[{"hour":1757289600,"calls":2,"input":9}],"by_model":{}}"#,
        )
        .expect("write good");

        let s = Store::load(&root);
        // The good day still loads: one bad file must not cost the history.
        assert_eq!(s.totals("ada", 0).0, 2, "the intact day should still be there");
        assert!(
            dir.join("2026-09-07_usage.json.corrupt").exists(),
            "the damaged file is kept for inspection, not deleted"
        );
        assert!(!dir.join("2026-09-07_usage.json").exists());
    }
}
