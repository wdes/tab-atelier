// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layered, clone-safe compaction of Claude Code JSONL transcripts.
//!
//! A conversation is an append-only JSONL tree — every line carries `uuid` and
//! `parentUuid`, and `claude --resume <session>` walks the leaf→root path back
//! to the API. The trims are independent LAYERS ([`Config`]); [`apply`] runs any
//! subset.
//!
//! ## Clone-safety
//!
//! [`apply`] takes **ownership** of the parsed records and mutates each
//! [`serde_json::Value`] in place — it `.take()`s the value out of the record,
//! edits that owned value, and puts it back. Transcript content (100 MB+ files,
//! multi-KB tool outputs) is never deep-`.clone()`d. Callers that need to try
//! several configs re-`parse` from the retained raw text rather than cloning
//! `Value`s.
//!
//! ## Resume-safety
//!
//! Layers A/D/E/F edit content *inside* a record (uuid/parentUuid untouched →
//! the message tree is structurally identical). Layers B/C drop records:
//! file-history records have no `uuid` and are never a `parentUuid` target
//! (direct drop); attachments can sit in the chain, so a dropped attachment is
//! **spliced out** — any surviving child is re-parented onto the attachment's
//! nearest surviving ancestor. [`validate`] proves no link that resolved before
//! now dangles.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// One transcript line: the original text plus its parsed value (`None` if the
/// line didn't parse — kept verbatim, never dropped).
pub struct Record {
    pub raw: String,
    pub value: Option<Value>,
}

/// The independently-activatable trim layers. `Default` = a no-op pass.
#[derive(Clone, Copy, Default)]
pub struct Config {
    /// A — drop the top-level `toolUseResult` metadata when a `tool_result`
    /// content block already carries the model-facing text `--resume` replays.
    pub dedup: bool,
    /// B — drop `file-history-snapshot` / `-delta` records (checkpoint machinery).
    pub drop_file_history: bool,
    /// C — drop `attachment` records (context re-injected each turn), splicing
    /// them out of the parent chain.
    pub drop_attachments: bool,
    /// D — strip `thinking` blocks from assistant turns older than the last K.
    pub keep_thinking: Option<usize>,
    /// E — truncate `tool_result` content larger than N bytes to head+tail.
    pub tool_cap: Option<usize>,
    /// F — blank base64 image data on turns older than the last K.
    pub keep_images: Option<usize>,
}

/// Bytes removed by each layer during an [`apply`] pass.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub dedup: u64,
    pub file_history: u64,
    pub attachments: u64,
    pub thinking: u64,
    pub cap: u64,
    pub images: u64,
}

impl Stats {
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.dedup + self.file_history + self.attachments + self.thinking + self.cap + self.images
    }
}

const FILE_HISTORY: [&str; 2] = ["file-history-snapshot", "file-history-delta"];

/// The named presets used by the dry-run report (and the future `compact <tab>`
/// default policy). Ordered lossless → aggressive.
#[must_use]
pub fn presets() -> Vec<(&'static str, Config)> {
    let base = Config {
        dedup: true,
        drop_file_history: true,
        drop_attachments: true,
        ..Config::default()
    };
    vec![
        ("lossless", base),
        (
            "balanced",
            Config {
                keep_thinking: Some(6),
                ..base
            },
        ),
        (
            "cap8k",
            Config {
                keep_thinking: Some(6),
                tool_cap: Some(8192),
                ..base
            },
        ),
        (
            "aggressive",
            Config {
                keep_thinking: Some(3),
                tool_cap: Some(4096),
                keep_images: Some(3),
                ..base
            },
        ),
    ]
}

/// Parse JSONL text into records (blank lines skipped, unparsable kept verbatim).
#[must_use]
pub fn parse(text: &str) -> Vec<Record> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| Record {
            raw: l.to_string(),
            value: serde_json::from_str(l).ok(),
        })
        .collect()
}

/// On-disk size the records serialize back to (raw bytes + one newline each).
#[must_use]
pub fn size(records: &[Record]) -> u64 {
    records.iter().map(|r| r.raw.len() as u64 + 1).sum()
}

/// The set of `uuid`s present in the records.
#[must_use]
pub fn uuid_set(records: &[Record]) -> HashSet<String> {
    records
        .iter()
        .filter_map(|r| r.value.as_ref()?.get("uuid")?.as_str().map(str::to_string))
        .collect()
}

fn rec_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn ser_len(v: &Value) -> u64 {
    serde_json::to_string(v).map_or(0, |s| s.len() as u64)
}

/// Truncate a UTF-8 string to a head+tail with a `…[trimmed N bytes]…` marker,
/// respecting char boundaries. Returns `(new, bytes_removed)`.
fn cap_string(s: &str, cap: usize) -> (String, u64) {
    if s.len() <= cap {
        return (s.to_string(), 0);
    }
    let mut head = cap * 3 / 4;
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = s.len() - (cap - (cap * 3 / 4));
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let trimmed = tail_start - head;
    let new = format!("{}\n...[trimmed {trimmed} bytes]...\n{}", &s[..head], &s[tail_start..]);
    let removed = (s.len() as u64).saturating_sub(new.len() as u64);
    (new, removed)
}

/// Run `cfg`'s enabled layers over `records`, returning the compacted records.
///
/// Also returns per-layer byte savings. Clone-safe: consumes `records` and
/// mutates each value in place. Deterministic and side-effect-free (no I/O).
#[must_use]
pub fn apply(records: Vec<Record>, cfg: &Config) -> (Vec<Record>, Stats) {
    let mut stats = Stats::default();

    // Borrow pass: parent map (for re-parenting), assistant turn indices (for
    // keep-last-K cutoffs), and the set of attachment uuids we'll splice out.
    let mut parent_of: HashMap<String, Option<String>> = HashMap::new();
    let mut assistant_idx: Vec<usize> = Vec::new();
    let mut dropped: HashSet<String> = HashSet::new();
    for (i, r) in records.iter().enumerate() {
        if let Some(v) = &r.value {
            if let Some(u) = v.get("uuid").and_then(Value::as_str) {
                parent_of.insert(
                    u.to_string(),
                    v.get("parentUuid").and_then(Value::as_str).map(str::to_string),
                );
                if cfg.drop_attachments && rec_type(v) == "attachment" {
                    dropped.insert(u.to_string());
                }
            }
            if rec_type(v) == "assistant" {
                assistant_idx.push(i);
            }
        }
    }
    // Records at index `i < cut` are "old" (trimmable). Keeping the last K
    // assistant turns ⇒ cut = index of the K-th-from-last assistant record.
    // K == 0 ⇒ keep nothing (cut = MAX); K ≥ turns ⇒ keep all (cut = 0).
    let cut = |keep: Option<usize>| -> Option<usize> {
        keep.map(|k| {
            if k == 0 {
                usize::MAX
            } else if assistant_idx.len() <= k {
                0
            } else {
                assistant_idx[assistant_idx.len() - k]
            }
        })
    };
    let cut_think = cut(cfg.keep_thinking);
    let cut_img = cut(cfg.keep_images);
    let resolve = |start: Option<String>| -> Option<String> {
        let mut pu = start;
        while let Some(u) = &pu {
            if dropped.contains(u) {
                pu = parent_of.get(u).cloned().flatten();
            } else {
                break;
            }
        }
        pu
    };

    let mut out = Vec::with_capacity(records.len());
    for (i, mut rec) in records.into_iter().enumerate() {
        // Move the value out so edits don't fight the borrow of `rec.raw`.
        let Some(mut v) = rec.value.take() else {
            out.push(rec);
            continue;
        };
        let ty = rec_type(&v).to_string();
        let uuid = v.get("uuid").and_then(Value::as_str).map(str::to_string);
        let raw_bytes = rec.raw.len() as u64 + 1;

        // B / direct-drop: file-history (no uuid) or a uuid-less attachment.
        if (cfg.drop_file_history && FILE_HISTORY.contains(&ty.as_str()))
            || (cfg.drop_attachments && ty == "attachment" && uuid.is_none())
        {
            if FILE_HISTORY.contains(&ty.as_str()) {
                stats.file_history += raw_bytes;
            } else {
                stats.attachments += raw_bytes;
            }
            continue;
        }
        // C / reparent-drop: an attachment that sits in the chain.
        if uuid.as_deref().is_some_and(|u| dropped.contains(u)) {
            stats.attachments += raw_bytes;
            continue;
        }

        let mut changed = false;

        // Re-parent onto the nearest surviving ancestor if our parent was dropped.
        if let Some(pu) = v.get("parentUuid").and_then(Value::as_str)
            && dropped.contains(pu)
        {
            let np = resolve(Some(pu.to_string()));
            if let Value::Object(m) = &mut v {
                m.insert("parentUuid".into(), np.map_or(Value::Null, Value::String));
                changed = true;
            }
        }

        // A: dedup — drop the top-level toolUseResult when a tool_result block exists.
        if cfg.dedup && v.get("toolUseResult").is_some() {
            let has_tr = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                });
            if has_tr {
                if let Some(tur) = v.get("toolUseResult") {
                    stats.dedup += ser_len(tur);
                }
                if let Value::Object(m) = &mut v {
                    m.remove("toolUseResult");
                    changed = true;
                }
            }
        }

        // D / E / F: content-block edits.
        if (cut_think.is_some() || cut_img.is_some() || cfg.tool_cap.is_some())
            && let Some(Value::Array(blocks)) = v.pointer_mut("/message/content")
        {
            let old = std::mem::take(blocks);
            let mut nb = Vec::with_capacity(old.len());
            for mut b in old {
                let bt = b.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                if bt == "thinking" && cut_think.is_some_and(|c| i < c) {
                    stats.thinking += ser_len(&b);
                    changed = true;
                    continue; // drop the thinking block
                }
                if bt == "image" && cut_img.is_some_and(|c| i < c) {
                    if let Some(data) = b.pointer("/source/data").and_then(Value::as_str)
                        && !data.is_empty()
                    {
                        stats.images += data.len() as u64;
                        if let Some(src) = b.pointer_mut("/source").and_then(Value::as_object_mut) {
                            src.insert("data".into(), Value::String(String::new()));
                            src.insert("_trimmed".into(), Value::Bool(true));
                            changed = true;
                        }
                    }
                    nb.push(b);
                    continue;
                }
                if bt == "tool_result"
                    && let Some(cap) = cfg.tool_cap
                    && let Some(c) = b.get("content")
                {
                    let ser = serde_json::to_string(c).unwrap_or_default();
                    if ser.len() > cap {
                        let (news, rm) = cap_string(&ser, cap);
                        if let Value::Object(m) = &mut b {
                            m.insert("content".into(), Value::String(news));
                        }
                        stats.cap += rm;
                        changed = true;
                    }
                }
                nb.push(b);
            }
            if let Some(Value::Array(blocks)) = v.pointer_mut("/message/content") {
                *blocks = nb;
            }
        }

        if changed {
            rec.raw = serde_json::to_string(&v).unwrap_or_else(|_| rec.raw.clone());
        }
        rec.value = Some(v);
        out.push(rec);
    }
    (out, stats)
}

/// Per-layer byte savings `cfg` WOULD reclaim, computed borrow-only.
///
/// No mutation, no allocation of a new transcript — ideal for scanning many
/// transcripts across many configs without re-parsing per config.
///
/// Each layer removes a DISJOINT byte region (top-level `toolUseResult` vs the
/// `tool_result` block vs `thinking` vs whole aux records vs image data), so the
/// per-layer totals are additive. It slightly UNDER-counts (ignores the few
/// bytes of key/comma wrapper each edit also removes), so it's a conservative
/// floor on the real on-disk saving.
#[must_use]
pub fn measure(records: &[Record], cfg: &Config) -> Stats {
    let mut stats = Stats::default();
    let assistant_idx: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.value.as_ref().is_some_and(|v| rec_type(v) == "assistant"))
        .map(|(i, _)| i)
        .collect();
    let cut = |keep: Option<usize>| -> Option<usize> {
        keep.map(|k| {
            if k == 0 {
                usize::MAX
            } else if assistant_idx.len() <= k {
                0
            } else {
                assistant_idx[assistant_idx.len() - k]
            }
        })
    };
    let cut_think = cut(cfg.keep_thinking);
    let cut_img = cut(cfg.keep_images);

    for (i, r) in records.iter().enumerate() {
        let Some(v) = &r.value else { continue };
        let ty = rec_type(v);
        let raw_bytes = r.raw.len() as u64 + 1;
        if cfg.drop_file_history && FILE_HISTORY.contains(&ty) {
            stats.file_history += raw_bytes;
            continue;
        }
        if cfg.drop_attachments && ty == "attachment" {
            stats.attachments += raw_bytes;
            continue;
        }
        if cfg.dedup && v.get("toolUseResult").is_some() {
            let has_tr = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                });
            if has_tr {
                stats.dedup += ser_len(v.get("toolUseResult").unwrap_or(&Value::Null));
            }
        }
        if let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "thinking" if cut_think.is_some_and(|c| i < c) => stats.thinking += ser_len(b),
                    "image" if cut_img.is_some_and(|c| i < c) => {
                        if let Some(d) = b.pointer("/source/data").and_then(Value::as_str) {
                            stats.images += d.len() as u64;
                        }
                    }
                    "tool_result" => {
                        if let Some(cap) = cfg.tool_cap {
                            let ser = b.get("content").map(|c| serde_json::to_string(c).unwrap_or_default());
                            if let Some(ser) = ser
                                && ser.len() > cap
                            {
                                stats.cap += cap_string(&ser, cap).1;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    stats
}

/// [`measure`] for many configs in a SINGLE pass.
///
/// Serializes each `thinking` / `toolUseResult` / `tool_result` block ONCE and
/// scores every config from the cached lengths — so scanning N configs costs
/// one serialization per block, not N. This is the scan the dry-run report uses
/// (10 configs × 54 files) to stay fast in a debug build.
#[must_use]
pub fn measure_batch(records: &[Record], configs: &[Config]) -> Vec<Stats> {
    let mut out = vec![Stats::default(); configs.len()];
    let assistant_idx: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.value.as_ref().is_some_and(|v| rec_type(v) == "assistant"))
        .map(|(i, _)| i)
        .collect();
    let cut = |keep: Option<usize>| -> Option<usize> {
        keep.map(|k| {
            if k == 0 {
                usize::MAX
            } else if assistant_idx.len() <= k {
                0
            } else {
                assistant_idx[assistant_idx.len() - k]
            }
        })
    };
    let cuts_think: Vec<Option<usize>> = configs.iter().map(|c| cut(c.keep_thinking)).collect();
    let cuts_img: Vec<Option<usize>> = configs.iter().map(|c| cut(c.keep_images)).collect();
    // The distinct tool-caps across configs, so a tool_result is capped once per
    // distinct threshold rather than once per config.
    let mut caps: Vec<usize> = configs.iter().filter_map(|c| c.tool_cap).collect();
    caps.sort_unstable();
    caps.dedup();

    for (i, r) in records.iter().enumerate() {
        let Some(v) = &r.value else { continue };
        let ty = rec_type(v);
        let raw_bytes = r.raw.len() as u64 + 1;
        let is_fh = FILE_HISTORY.contains(&ty);
        let is_att = ty == "attachment";

        // --- serialize-once, reusable per-record quantities ---
        let dedup_size = if v.get("toolUseResult").is_some()
            && v.get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                }) {
            ser_len(v.get("toolUseResult").unwrap_or(&Value::Null))
        } else {
            0
        };
        let mut thinking_total = 0u64;
        let mut image_total = 0u64;
        let mut cap_removed: Vec<u64> = vec![0; caps.len()]; // parallel to `caps`
        if let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "thinking" => thinking_total += ser_len(b),
                    "image" => {
                        if let Some(d) = b.pointer("/source/data").and_then(Value::as_str) {
                            image_total += d.len() as u64;
                        }
                    }
                    "tool_result" => {
                        if !caps.is_empty()
                            && let Some(c) = b.get("content")
                        {
                            let ser = serde_json::to_string(c).unwrap_or_default();
                            for (ci, &cap) in caps.iter().enumerate() {
                                if ser.len() > cap {
                                    cap_removed[ci] += cap_string(&ser, cap).1;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // --- score each config from the cached quantities ---
        for (ci, cfg) in configs.iter().enumerate() {
            if (cfg.drop_file_history && is_fh) || (cfg.drop_attachments && is_att) {
                if is_fh {
                    out[ci].file_history += raw_bytes;
                } else {
                    out[ci].attachments += raw_bytes;
                }
                continue; // dropped for this config → no content edits counted
            }
            if cfg.dedup {
                out[ci].dedup += dedup_size;
            }
            if cuts_think[ci].is_some_and(|c| i < c) {
                out[ci].thinking += thinking_total;
            }
            if cuts_img[ci].is_some_and(|c| i < c) {
                out[ci].images += image_total;
            }
            if let Some(cap) = cfg.tool_cap
                && let Ok(idx) = caps.binary_search(&cap)
            {
                out[ci].cap += cap_removed[idx];
            }
        }
    }
    out
}

/// Diff-based integrity check: the problems a transform INTRODUCED.
///
/// A dangling `parentUuid` is only a fault if that parent existed in the
/// original (we dropped it without re-parenting); parents that never existed
/// are pre-existing external / post-compact roots and are left alone. Empty ⇒
/// `--resume` integrity preserved.
#[must_use]
pub fn validate<S: std::hash::BuildHasher>(orig_uuids: &HashSet<String, S>, new: &[Record]) -> Vec<String> {
    let new_uuids = uuid_set(new);
    let mut problems = Vec::new();
    for r in new {
        let Some(v) = &r.value else {
            problems.push("unparsable line survived".to_string());
            continue;
        };
        if let Some(pu) = v.get("parentUuid").and_then(Value::as_str)
            && !new_uuids.contains(pu)
            && orig_uuids.contains(pu)
        {
            let child = v.get("uuid").and_then(Value::as_str).unwrap_or("?");
            problems.push(format!("broke link {child} -> {pu} (dropped without re-parent)"));
            if problems.len() >= 3 {
                break;
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(json: &str) -> String {
        json.to_string()
    }

    // A tiny transcript: user → assistant(thinking+tool_use) → attachment →
    // user(tool_result + toolUseResult) → file-history.
    fn sample() -> String {
        [
            rec(r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"content":[{"type":"text","text":"hi"}]}}"#),
            rec(r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"thinking","thinking":"deep thoughts here"},{"type":"tool_use","id":"t1","name":"Bash"}]}}"#),
            rec(r#"{"type":"attachment","uuid":"att1","parentUuid":"a1","attachment":{"type":"task_reminder"}}"#),
            rec(r#"{"type":"user","uuid":"u2","parentUuid":"att1","toolUseResult":{"stdout":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}]}}"#),
            rec(r#"{"type":"file-history-snapshot","messageId":"a1","snapshot":{}}"#),
        ]
        .join("\n")
    }

    #[test]
    fn dedup_removes_tooluseresult_keeps_block() {
        let recs = parse(&sample());
        let (out, st) = apply(
            recs,
            &Config {
                dedup: true,
                ..Config::default()
            },
        );
        assert!(st.dedup > 0);
        let u2 = out.iter().find(|r| r.raw.contains("\"u2\"")).unwrap();
        assert!(!u2.raw.contains("toolUseResult"), "metadata copy dropped");
        assert!(u2.raw.contains("tool_result"), "model-facing block kept");
    }

    #[test]
    fn drop_file_history_and_attachment_reparent() {
        let orig = parse(&sample());
        let orig_uuids = uuid_set(&orig);
        let cfg = Config {
            drop_file_history: true,
            drop_attachments: true,
            ..Config::default()
        };
        let (out, st) = apply(orig, &cfg);
        assert!(st.file_history > 0 && st.attachments > 0);
        assert!(!out.iter().any(|r| r.raw.contains("file-history")), "fh dropped");
        assert!(!out.iter().any(|r| r.raw.contains("\"att1\"")), "attachment dropped");
        // u2's parent was att1 (dropped) → must be re-parented to a1 (att1's parent).
        let u2 = out.iter().find(|r| r.raw.contains("\"u2\"")).unwrap();
        let v: Value = serde_json::from_str(&u2.raw).unwrap();
        assert_eq!(v["parentUuid"], "a1", "child spliced onto surviving ancestor");
        assert!(validate(&orig_uuids, &out).is_empty(), "no links broken");
    }

    #[test]
    fn strip_thinking_keeps_recent_and_stays_resumable() {
        let orig = parse(&sample());
        let orig_uuids = uuid_set(&orig);
        // keep_thinking: 0 → strip all thinking (a1 is the only assistant turn).
        let (out, st) = apply(
            parse(&sample()),
            &Config {
                keep_thinking: Some(0),
                ..Config::default()
            },
        );
        assert!(st.thinking > 0);
        assert!(
            !out.iter().any(|r| r.raw.contains("deep thoughts")),
            "old thinking stripped"
        );
        assert!(
            out.iter().any(|r| r.raw.contains("tool_use")),
            "tool_use in same turn kept"
        );
        assert!(validate(&orig_uuids, &out).is_empty());
    }

    #[test]
    fn tool_cap_truncates_big_output() {
        let big = "X".repeat(5000);
        let line = format!(
            r#"{{"type":"user","uuid":"u3","parentUuid":null,"message":{{"content":[{{"type":"tool_result","content":"{big}"}}]}}}}"#
        );
        let (out, st) = apply(
            parse(&line),
            &Config {
                tool_cap: Some(512),
                ..Config::default()
            },
        );
        assert!(st.cap > 4000, "trimmed most of it: {}", st.cap);
        assert!(out[0].raw.contains("[trimmed"), "marker present");
        assert!(out[0].raw.len() < 1500);
    }

    #[test]
    fn measure_batch_matches_individual_measure() {
        let recs = parse(&sample());
        let cfgs: Vec<Config> = presets().into_iter().map(|(_, c)| c).collect();
        let batch = measure_batch(&recs, &cfgs);
        for (i, c) in cfgs.iter().enumerate() {
            let solo = measure(&recs, c);
            assert_eq!(batch[i].total(), solo.total(), "preset {i} batch vs solo");
            assert_eq!(batch[i].dedup, solo.dedup);
            assert_eq!(batch[i].file_history, solo.file_history);
            assert_eq!(batch[i].attachments, solo.attachments);
            assert_eq!(batch[i].thinking, solo.thinking);
            assert_eq!(batch[i].cap, solo.cap);
            assert_eq!(batch[i].images, solo.images);
        }
    }

    #[test]
    fn noop_config_changes_nothing() {
        let text = sample();
        let (out, st) = apply(parse(&text), &Config::default());
        assert_eq!(st.total(), 0);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn cap_string_respects_char_boundaries() {
        let s = "é".repeat(1000); // 2 bytes each
        let (new, removed) = cap_string(&s, 100);
        assert!(removed > 0);
        assert!(new.is_char_boundary(0));
        // Round-trips as valid UTF-8 (no panic slicing mid-char).
        assert!(new.contains("[trimmed"));
    }
}
