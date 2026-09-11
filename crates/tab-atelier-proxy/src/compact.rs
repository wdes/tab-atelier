// SPDX-License-Identifier: MPL-2.0

//! Deterministic body compaction — see `docs/proxy-compaction.md`.
//!
//! A Claude Code request is the whole conversation, resent every turn. On the
//! Anthropic hop that is *fine*: `cache_control` breakpoints make everything
//! before the last one a cache read, and any byte change anywhere in the
//! prefix invalidates every breakpoint after it — so rewriting the middle of
//! `messages` there turns every turn into a full-price cache miss. Shorter
//! body, bigger bill.
//!
//! A prompt cache does not follow a request to a provider that is not
//! Anthropic, though. The moment routing sends a call elsewhere the cache is
//! gone anyway, and *that* is the hop where shortening the history costs
//! nothing. Hence a per-provider setting rather than a global one.
//!
//! # Deterministic, and why that matters
//!
//! Identical input bytes produce identical output bytes: no clock, no
//! randomness, nothing that drifts between two runs of the same request. A
//! compactor that varied would re-warm a fresh prefix on every turn, which is
//! the exact cost this exists to avoid — so the windows below are counted from
//! the end of the array and the stubs are pure functions of what they replace.
//!
//! # What is never touched
//!
//! `tools[]` sits at the front of the cache prefix and editing a schema can
//! desync the `tool_use` arguments the model already emitted. `system[]` is
//! the other half of that prefix. Neither is in the blast radius here, nor is
//! `metadata`, `context_management`, `thinking` or `output_config`.
//!
//! # Relationship to `src/transcript_compact.rs`
//!
//! The desktop crate compacts stored JSONL transcripts; this compacts what is
//! sent, and the two cannot share code — different input shapes, different
//! operation (a byte cap on a tool output there, whole-block elision with a
//! stub here), and the proxy deliberately does not depend on the desktop
//! crate, which would drag a GUI toolkit into a server. What they do share is
//! the invariant, and it is the one thing that must not drift: **a `tool_use`
//! and its `tool_result` are a pair, and a dropped `tool_result` leaves its
//! `tool_use` unanswered, which is a 400.** Nothing here removes
//! `tool_result` blocks; it replaces their contents.

use serde::{Deserialize, Serialize};

/// How many trailing turns each layer leaves alone.
///
/// Six because that is roughly the span the model is still reasoning over —
/// enough that nothing in flight gets elided from under it, small enough that
/// a long session sheds most of its bulk.
pub const KEEP_TURNS: usize = 6;

/// What a provider's compaction pass removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compact {
    /// The default, and the correct value on the Anthropic hop.
    #[default]
    None,
    /// Layer A: stub out old `tool_result` contents.
    Tools,
    /// Layer B: A, plus drop `thinking` from old assistant turns.
    ToolsThinking,
    /// Layer D: B, plus stub the file bodies old `Write`/`Edit` calls carry.
    ///
    /// Sits below [`Compact::All`] rather than above it because layer C is the
    /// cheapest thing here — a stale token banner is noise, not bulk — and the
    /// ladder is ordered by how much a level removes. `Writes` is the bulk of
    /// layers A–D without the banner edit; `All` is all four.
    Writes,
    /// Layer C on top of the rest: stale `<total_tokens>` banners go too.
    All,
}

impl Compact {
    /// Every level, in order of how much they remove.
    pub const ALL: [Self; 5] = [Self::None, Self::Tools, Self::ToolsThinking, Self::Writes, Self::All];

    /// The name as it appears in `providers.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tools => "tools",
            Self::ToolsThinking => "tools_thinking",
            Self::Writes => "writes",
            Self::All => "all",
        }
    }

    /// What the admin UI offers, in the same order as [`Compact::ALL`].
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Tools => "Remove old tool results",
            Self::ToolsThinking => "Remove old tool results and thinking",
            Self::Writes => "Remove old tool results, thinking and Write/Edit payloads",
            Self::All => "Remove old tool results, thinking, Write/Edit payloads and banners",
        }
    }

    /// Whether this level stubs out old tool results.
    #[must_use]
    pub const fn does_tools(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this level drops old thinking blocks.
    #[must_use]
    pub const fn does_thinking(self) -> bool {
        matches!(self, Self::ToolsThinking | Self::Writes | Self::All)
    }

    /// Whether this level stubs out the file bodies old write calls carry.
    #[must_use]
    pub const fn does_writes(self) -> bool {
        matches!(self, Self::Writes | Self::All)
    }

    /// Whether this level drops stale token banners.
    #[must_use]
    pub const fn does_banners(self) -> bool {
        matches!(self, Self::All)
    }

    /// Whether this level leaves the body alone entirely.
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// What one pass did, for the log line.
///
/// Counts only. The caller already holds the request bytes on both sides of
/// the pass, so measuring here would mean serializing the body twice more to
/// report a number the caller can read off `len()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub tool_results_elided: usize,
    /// Tool results left alone because they carry an error the model may
    /// still need to reason about — see [`elide_tool_results`].
    pub tool_results_kept_for_error: usize,
    pub thinking_dropped: usize,
    /// Layer D: file bodies replaced inside old `tool_use` inputs.
    ///
    /// Counted per STRING, not per call, because one call can carry several —
    /// an `Edit` has `old_string` and `new_string`, a `MultiEdit` has a whole
    /// array. A count of calls would read as "one file" on a turn that dropped
    /// two.
    pub writes_elided: usize,
    pub banners_dropped: usize,
}

impl Stats {
    /// Whether the pass changed anything worth logging.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.tool_results_elided > 0 || self.thinking_dropped > 0 || self.writes_elided > 0 || self.banners_dropped > 0
    }
}

/// The bytes a content block occupies once serialized.
///
/// The serialized length, not the string length: `content` is a string in some
/// requests and an array of blocks in others, and only the serialized form is
/// defined for both. It is also the number the stub is asked for — how much
/// this call's result cost before it was elided.
fn bytes_of(value: &serde_json::Value) -> u64 {
    u64::try_from(serde_json::to_vec(value).map_or(0, |v| v.len())).unwrap_or(u64::MAX)
}

/// The marker every stub begins with.
///
/// Load-bearing, not cosmetic: it is what makes the pass **idempotent**. A
/// second pass over an already-compacted body would otherwise elide the stubs
/// again and recompute their byte count from the stub instead of from the
/// result it replaced — so the number in the message would shrink on every
/// pass and the original size would be gone. A retry, or a second route change
/// on the same request, is enough to do it.
const ELIDED_PREFIX: &str = "[tool result elided by tab-atelier-proxy: ";

/// The stub that replaces an elided `tool_result`'s content.
///
/// The block, its id and its position all stay. That is the point: the model
/// is told *something was there* and which call it answered, rather than being
/// shown an empty result and concluding the tool returned nothing.
fn stub(byte_count: u64, tool_use_id: &str) -> String {
    format!("{ELIDED_PREFIX}{byte_count} bytes; tool_use_id={tool_use_id}]")
}

/// Whether this content is already a stub from an earlier pass.
fn already_elided(content: &serde_json::Value) -> bool {
    content.as_str().is_some_and(|s| s.starts_with(ELIDED_PREFIX))
}

/// The tools whose `input` carries file content rather than a command.
///
/// Matched by NAME, which is a coupling the other layers do not have: A and B
/// read the block `type`, which the API defines, while these strings are Claude
/// Code's choice and could be renamed. The failure is bounded and safe — a
/// renamed tool simply stops being compacted, exactly as it is today — and the
/// alternative, matching on the SHAPE of `input`, would rewrite whichever tool
/// happened to have a long field in it. A false positive there corrupts a call
/// the model still needs; a false negative costs bytes.
const WRITE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// The smallest string worth replacing.
///
/// Well above the length of a path, a hash or a flag, and well below a file
/// body. Charging a stub for a 30-byte `old_string` would make the body BIGGER
/// on a turn that edits four small things.
const MIN_WRITE_BYTES: usize = 200;

/// The marker every write stub begins with — see [`ELIDED_PREFIX`] for why the
/// pass needs one at all.
const WRITE_ELIDED_PREFIX: &str = "[file content elided by tab-atelier-proxy: ";

/// The stub that replaces a string inside a `tool_use` `input`.
///
/// It names no path: the `file_path` field beside it is untouched and already
/// says which file, and repeating it would mean the stub disagreed with reality
/// whenever the pass had rewritten the one and not the other. The byte count is
/// the serialized length, the same quantity `bytes_of` reports for layer A.
fn write_stub(byte_count: u64) -> String {
    format!("{WRITE_ELIDED_PREFIX}{byte_count} bytes]")
}

/// The oldest message index still inside the trailing window.
///
/// Counted from the end over messages that `qualifies` — that is, over the
/// turns this layer actually acts on. Counting *all* messages instead would
/// make the window shrink whenever a turn happened to contain nothing of the
/// relevant kind, which is how a "keep 6" rule quietly keeps 3.
fn window_start(messages: &[serde_json::Value], keep: usize, qualifies: impl Fn(&serde_json::Value) -> bool) -> usize {
    let mut seen = 0;
    for i in (0..messages.len()).rev() {
        if qualifies(&messages[i]) {
            seen += 1;
            if seen == keep {
                return i;
            }
        }
    }
    0
}

/// Whether a message carries at least one block of `kind`.
fn has_block(message: &serde_json::Value, kind: &str) -> bool {
    blocks(message).is_some_and(|b| b.iter().any(|block| block_type(block) == Some(kind)))
}

/// The block type of a content block, if it is an object with one.
fn block_type(block: &serde_json::Value) -> Option<&str> {
    block.get("type").and_then(serde_json::Value::as_str)
}

/// A message's content, when it is the array-of-blocks form.
///
/// A string content has no blocks to walk; it is not a tool result and not
/// thinking, so every layer simply skips it.
fn blocks(message: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    message.get("content").and_then(serde_json::Value::as_array)
}

fn blocks_mut(message: &mut serde_json::Value) -> Option<&mut Vec<serde_json::Value>> {
    message.get_mut("content").and_then(serde_json::Value::as_array_mut)
}

/// Layer A — stub the content of old `tool_result` blocks.
fn elide_tool_results(messages: &mut [serde_json::Value], stats: &mut Stats) {
    let start = window_start(messages, KEEP_TURNS, |m| has_block(m, "tool_result"));
    for message in &mut messages[..start] {
        let Some(list) = blocks_mut(message) else { continue };
        for block in list.iter_mut() {
            if block_type(block) != Some("tool_result") {
                continue;
            }
            // An elided error is the one elision class where the loss is
            // semantic rather than bulk — "use the Grep tool instead" is not
            // something the model can re-derive from a byte count. A few KB
            // is a cheap price for never doing that.
            let is_error = block
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if is_error {
                stats.tool_results_kept_for_error += 1;
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(content) = block.get("content") else { continue };
            // Already a stub from a previous pass: leaving it alone keeps the
            // original byte count in the message and makes the pass idempotent.
            if already_elided(content) {
                continue;
            }
            let replacement = stub(bytes_of(content), id);
            block["content"] = serde_json::Value::String(replacement);
            stats.tool_results_elided += 1;
        }
    }
}

/// Layer B — drop `thinking` blocks from old assistant turns.
///
/// The window is counted over messages that *contain* thinking, not over every
/// assistant message. `thinking` blocks are the thing being kept, so counting
/// turns that have none would shrink the window by exactly the number of turns
/// it did not need to protect.
fn drop_thinking(messages: &mut [serde_json::Value], stats: &mut Stats) {
    let start = window_start(messages, KEEP_TURNS, |m| has_block(m, "thinking"));
    for message in &mut messages[..start] {
        let Some(list) = blocks_mut(message) else { continue };
        let before = list.len();
        list.retain(|block| block_type(block) != Some("thinking"));
        stats.thinking_dropped += before - list.len();
    }
}

/// Layer D — stub the file bodies old write calls carry in their `input`.
///
/// The mirror of layer A, and the same bulk seen from the other side. A `Write`
/// block holds the whole file in its `input.content`, while its matching
/// `tool_result` is the four words "File created successfully" — so layer A
/// finds nothing there while the payload sits in the pair's other half. On an
/// editing session that is the largest untouched class in the body.
///
/// Only the VALUE changes, never the key. Upstream validates a re-sent
/// `tool_use` block's `input` against the tool's schema on every turn, so the
/// field has to stay present and stay a string: shrinking a string cannot fail
/// a `type: string` check, while removing the key fails `required`. The
/// `tool_use` block itself, its id, its name and its position all stay — which
/// is also what keeps the id-pairing invariant of layer A intact.
fn elide_writes(messages: &mut [serde_json::Value], stats: &mut Stats) {
    let start = window_start(messages, KEEP_TURNS, has_write_call);
    for message in &mut messages[..start] {
        let Some(list) = blocks_mut(message) else { continue };
        for block in list.iter_mut() {
            if !is_write_call(block) {
                continue;
            }
            // `file_path`, `cell_id` and the rest of the small fields survive;
            // only the long strings go. There is no error case to exempt here
            // — a `tool_use` has no `is_error`, and a call that FAILED still
            // has its reason in the `tool_result`, which layer A treats.
            let Some(input) = block.get_mut("input") else { continue };
            stats.writes_elided += elide_long_strings(input);
        }
    }
}

/// Whether a content block is a `tool_use` for one of the file-writing tools.
fn is_write_call(block: &serde_json::Value) -> bool {
    block_type(block) == Some("tool_use")
        && block
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| WRITE_TOOLS.contains(&name))
}

/// Whether a message carries at least one write call.
fn has_write_call(message: &serde_json::Value) -> bool {
    blocks(message).is_some_and(|b| b.iter().any(is_write_call))
}

/// Replace every long string anywhere inside a `tool_use` `input`, returning
/// how many were replaced.
///
/// Recursive rather than a fixed list of field names, because `MultiEdit` nests
/// its edits in an array and a tool may nest its payload one level deeper than
/// any list written here could predict. Depth is bounded by the JSON the caller
/// already parsed, so this cannot recurse further than the body itself does.
fn elide_long_strings(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => {
            // Idempotence, deliberately ahead of the size gate. The stub is
            // well under `MIN_WRITE_BYTES` today, so the gate would also stop
            // it — but that would make the pass idempotent by arithmetic
            // coincidence rather than by rule, and a second pass would then
            // recompute the byte count from the stub, losing the original.
            if s.starts_with(WRITE_ELIDED_PREFIX) {
                return 0;
            }
            if s.len() < MIN_WRITE_BYTES {
                return 0;
            }
            let serialized = serde_json::to_vec(&*s).map_or(0, |v| v.len());
            *s = write_stub(u64::try_from(serialized).unwrap_or(u64::MAX));
            1
        }
        serde_json::Value::Array(items) => items.iter_mut().map(elide_long_strings).sum(),
        serde_json::Value::Object(fields) => fields.values_mut().map(elide_long_strings).sum(),
        _ => 0,
    }
}

/// Layer C — drop stale `<total_tokens>` banners, keeping the newest.
///
/// Worth almost nothing in bytes — 23 messages at ~49 B is about a kilobyte.
/// Kept because a stale token count re-read 23 times is noise rather than
/// context, and noise is what the model has least room for.
fn drop_banners(messages: &mut Vec<serde_json::Value>, stats: &mut Stats) {
    // Walked from the end, so the first banner met is the newest and every
    // later removal is at a HIGHER index than any still to come — which is
    // what makes removing while iterating safe here.
    let mut kept_newest = false;
    let mut index = messages.len();
    while index > 0 {
        index -= 1;
        if !is_banner(&messages[index]) {
            continue;
        }
        if kept_newest {
            messages.remove(index);
            stats.banners_dropped += 1;
        } else {
            kept_newest = true;
        }
    }
}

/// A `<total_tokens>…</total_tokens>` message Claude Code re-injects each turn.
fn is_banner(message: &serde_json::Value) -> bool {
    message.get("role").and_then(serde_json::Value::as_str) == Some("system")
        && message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|c| c.starts_with("<total_tokens>"))
}

/// Run the pass for `level`, reporting what it did.
///
/// A body that is not an object with a `messages` array is left exactly as it
/// arrived: this is a best-effort rewrite of a request the proxy did not
/// author, and refusing one over a shape it does not have would turn a size
/// optimisation into an outage.
#[must_use]
pub fn apply(body: &mut serde_json::Value, level: Compact) -> Stats {
    let mut stats = Stats::default();
    if level.is_none() {
        return stats;
    }
    let Some(messages) = body.get_mut("messages").and_then(serde_json::Value::as_array_mut) else {
        return stats;
    };

    if level.does_tools() {
        elide_tool_results(messages, &mut stats);
    }
    if level.does_thinking() {
        drop_thinking(messages, &mut stats);
    }
    if level.does_writes() {
        elide_writes(messages, &mut stats);
    }
    if level.does_banners() {
        drop_banners(messages, &mut stats);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// How many turns the fixtures below build.
    const TURNS: usize = 10;

    /// A body in the shape Claude Code sends: a system prompt, tool schemas,
    /// and alternating turns — a user turn carrying a `tool_result`, an
    /// assistant turn carrying `thinking` and the `tool_use` it answers.
    ///
    /// Every part the pass must not touch is present, because "we did not
    /// change it" is only worth asserting when there was something there to
    /// change.
    fn body() -> serde_json::Value {
        let mut messages = Vec::new();
        for i in 0..TURNS {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("call_{i:02}"),
                    // Distinct sizes, so the stub's byte count is checkable.
                    "content": "r".repeat(1000 + i),
                }],
            }));
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "t".repeat(200 + i), "signature": format!("sig{i:02}")},
                    {"type": "tool_use", "id": format!("call_{i:02}"), "name": "Bash", "input": {"command": "ls"}},
                ],
            }));
        }
        json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "metadata": {"user_id": "abc"},
            "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "output_config": {"effort": "high"},
            "system": [{"type": "text", "text": "you are claude code"}],
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}}],
            "messages": messages,
        })
    }

    fn messages(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["messages"].as_array().expect("messages")
    }

    /// Every `tool_result` block, in order.
    fn tool_results(body: &serde_json::Value) -> Vec<&serde_json::Value> {
        messages(body)
            .iter()
            .filter_map(blocks)
            .flatten()
            .filter(|b| block_type(b) == Some("tool_result"))
            .collect()
    }

    fn thinking(body: &serde_json::Value) -> Vec<&serde_json::Value> {
        messages(body)
            .iter()
            .filter_map(blocks)
            .flatten()
            .filter(|b| block_type(b) == Some("thinking"))
            .collect()
    }

    /// The `tool_use` and `tool_result` id sets, which must stay equal.
    fn pair_ids(body: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let mut uses = Vec::new();
        let mut results = Vec::new();
        for block in messages(body).iter().filter_map(blocks).flatten() {
            match block_type(block) {
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(serde_json::Value::as_str) {
                        uses.push(id.to_owned());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) {
                        results.push(id.to_owned());
                    }
                }
                _ => {}
            }
        }
        uses.sort_unstable();
        results.sort_unstable();
        (uses, results)
    }

    fn serialized(v: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(v).expect("serialize")
    }

    fn apply_to(level: Compact) -> (serde_json::Value, Stats) {
        let mut b = body();
        let stats = apply(&mut b, level);
        (b, stats)
    }

    #[test]
    fn none_leaves_the_body_exactly_as_it_arrived() {
        let mut b = body();
        let before = serialized(&b);
        let stats = apply(&mut b, Compact::None);
        assert_eq!(serialized(&b), before, "the default must be the identity");
        assert!(!stats.changed());
        assert_eq!(stats, Stats::default());
    }

    /// Layer A. The block stays, its id stays, its position stays — only the
    /// content becomes a stub that says what was there.
    #[test]
    fn layer_a_stubs_old_tool_results_and_keeps_every_block() {
        let (after, stats) = apply_to(Compact::Tools);
        let results = tool_results(&after);
        assert_eq!(results.len(), TURNS, "no tool_result block may be removed");
        assert_eq!(stats.tool_results_elided, TURNS - KEEP_TURNS);
        assert_eq!(stats.thinking_dropped, 0, "layer A does not touch thinking");

        // The elided ones carry the stub, with the byte count of what they
        // replaced and the id they answer.
        let first = results[0];
        assert_eq!(first["tool_use_id"], "call_00");
        let text = first["content"].as_str().expect("content became a string");
        assert_eq!(
            text,
            format!(
                "[tool result elided by tab-atelier-proxy: {} bytes; tool_use_id=call_00]",
                // The serialized length of the 1000-byte payload plus quotes.
                1002
            )
        );
        assert!(first.get("is_error").is_none(), "the block is otherwise untouched");

        // …and the kept ones are untouched, byte for byte.
        let before = body();
        let originals = tool_results(&before);
        for kept in results.iter().skip(TURNS - KEEP_TURNS) {
            let id = kept["tool_use_id"].as_str().expect("id");
            let original = originals
                .iter()
                .find(|b| b["tool_use_id"] == id)
                .expect("an original with this id");
            assert_eq!(
                serialized(kept),
                serialized(original),
                "the kept tool results must be byte-identical: {id}"
            );
        }
    }

    /// The one elision class where the loss is semantic rather than bulk.
    #[test]
    fn layer_a_never_elides_an_error() {
        let mut b = body();
        // Make the OLDEST tool result an error — well outside the window, so
        // only the is_error rule can save it.
        b["messages"][0]["content"][0]["is_error"] = json!(true);
        let stats = apply(&mut b, Compact::Tools);

        assert_eq!(stats.tool_results_elided, TURNS - KEEP_TURNS - 1);
        assert_eq!(stats.tool_results_kept_for_error, 1);
        let first = tool_results(&b)[0];
        assert_eq!(
            serialized(&first["content"]).len(),
            1002,
            "the error's content is intact, not stubbed"
        );
    }

    /// Layer B, and its window: counted over turns that HAVE thinking, so
    /// "keep 6" keeps six of them rather than six assistant messages.
    #[test]
    fn layer_b_drops_only_old_thinking_and_keeps_six() {
        let (after, stats) = apply_to(Compact::ToolsThinking);
        assert_eq!(stats.thinking_dropped, TURNS - KEEP_TURNS);
        assert_eq!(thinking(&after).len(), KEEP_TURNS);

        let before = body();
        let originals = thinking(&before);
        for kept in thinking(&after) {
            let sig = kept["signature"].as_str().expect("signature");
            let original = originals
                .iter()
                .find(|b| b["signature"] == sig)
                .expect("an original with this signature");
            assert_eq!(
                serialized(kept),
                serialized(original),
                "the kept thinking must be byte-identical: {sig}"
            );
        }
        // Layer B is cumulative: A ran too.
        assert_eq!(stats.tool_results_elided, TURNS - KEEP_TURNS);
    }

    /// A message that is only a thinking block must not vanish with it.
    #[test]
    fn layer_b_leaves_the_message_when_it_drops_its_whole_content() {
        let mut b = body();
        b["messages"][1] = json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "alone", "signature": "s"}],
        });
        let _ = apply(&mut b, Compact::ToolsThinking);
        assert_eq!(messages(&b).len(), TURNS * 2, "the turn is kept");
        assert!(
            b["messages"][1]["content"].as_array().expect("array").is_empty(),
            "with its content emptied, not the message dropped"
        );
    }

    /// Layer C keeps the newest banner and drops the stale ones.
    #[test]
    fn layer_c_keeps_only_the_newest_token_banner() {
        let mut b = body();
        let mut msgs = messages(&b).clone();
        for i in 0..3 {
            msgs.insert(
                1 + i * 2,
                json!({"role": "system", "content": format!("<total_tokens>{i}</total_tokens>")}),
            );
        }
        b["messages"] = json!(msgs);

        let stats = apply(&mut b, Compact::All);
        assert_eq!(stats.banners_dropped, 2);
        let banners: Vec<&str> = messages(&b)
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(banners, vec!["<total_tokens>2</total_tokens>"], "the newest survives");

        // A `system` message that is not a banner is not a banner.
        let mut b = body();
        let mut msgs = messages(&b).clone();
        msgs.insert(0, json!({"role": "system", "content": "a real instruction"}));
        b["messages"] = json!(msgs);
        let stats = apply(&mut b, Compact::All);
        assert_eq!(stats.banners_dropped, 0);
        assert_eq!(messages(&b)[0]["content"], "a real instruction");
    }

    /// The invariant the whole feature is one mistake away from breaking: a
    /// `tool_use` with no `tool_result` is a 400 from upstream.
    #[test]
    fn the_pairing_survives_every_layer() {
        let (before_uses, before_results) = pair_ids(&body());
        assert_eq!(before_uses, before_results, "the fixture is balanced to begin with");
        assert_eq!(before_uses.len(), TURNS);

        for level in Compact::ALL {
            let (after, _) = apply_to(level);
            let (uses, results) = pair_ids(&after);
            assert_eq!(uses, before_uses, "{level:?} changed the tool_use ids");
            assert_eq!(results, before_results, "{level:?} changed the tool_result ids");
            assert_eq!(uses, results, "{level:?} unbalanced the pairing");
        }
    }

    /// The cache root: editing any of this desyncs the tool schema the model
    /// already emitted arguments against, or the prefix everything else hangs
    /// off. Hashing the serialized form is the check that nothing here moved.
    #[test]
    fn the_cache_prefix_and_control_fields_are_untouched() {
        let before = body();
        let (after, _) = apply_to(Compact::All);
        for field in [
            "tools",
            "system",
            "metadata",
            "context_management",
            "thinking",
            "output_config",
            "max_tokens",
            "model",
        ] {
            assert_eq!(
                serialized(&before[field]),
                serialized(&after[field]),
                "{field} was modified"
            );
        }
    }

    /// Identical input bytes produce identical output bytes — no clock, no
    /// randomness. Otherwise the rewrite re-warms a fresh cache prefix every
    /// turn, which is the cost this exists to avoid.
    #[test]
    fn the_pass_is_deterministic() {
        let mut first = body();
        let _ = apply(&mut first, Compact::All);
        for _ in 0..3 {
            let mut again = body();
            let _ = apply(&mut again, Compact::All);
            assert_eq!(serialized(&first), serialized(&again));
        }
        // And on an already-compacted body. This is not a hypothetical: a
        // retry, or a second route change on the same request, re-runs the
        // pass over its own output. Without the stub marker it elided the
        // stubs again and recomputed their byte count from the stub — so the
        // number in the message shrank on every pass and the size of the
        // result it replaced was gone.
        let mut twice = first.clone();
        let stats = apply(&mut twice, Compact::All);
        assert_eq!(serialized(&twice), serialized(&first), "idempotent");
        assert_eq!(stats.tool_results_elided, 0, "and reports nothing to do");

        // The count in the stub is the size of what it replaced, not of the
        // stub — which is the shape the bug above took.
        let stubs: Vec<&str> = tool_results(&first)
            .iter()
            .filter_map(|b| b["content"].as_str())
            .filter(|c| c.starts_with(ELIDED_PREFIX))
            .collect();
        assert_eq!(stubs.len(), TURNS - KEEP_TURNS);
        assert!(stubs[0].contains("1002 bytes"), "{}", stubs[0]);
        assert!(
            !stubs[0].contains("76 bytes"),
            "the stub's own length leaked in: {}",
            stubs[0]
        );
    }

    /// A body the pass does not understand is forwarded, not refused: this is
    /// a size optimisation on a request the proxy did not author.
    #[test]
    fn a_body_without_the_shape_is_left_alone() {
        for odd in [
            json!({"model": "m"}),
            json!({"messages": "not an array"}),
            json!({"messages": []}),
            json!("a string"),
            json!(null),
        ] {
            let mut b = odd.clone();
            let stats = apply(&mut b, Compact::All);
            assert_eq!(serialized(&b), serialized(&odd));
            assert!(!stats.changed());
        }
    }

    /// A string `content` has no blocks to walk and must be left alone.
    #[test]
    fn a_string_content_is_not_mistaken_for_a_block_list() {
        let mut b = json!({
            "messages": [
                {"role": "user", "content": "just text"},
                {"role": "assistant", "content": "an answer"},
            ],
        });
        let before = serialized(&b);
        let stats = apply(&mut b, Compact::All);
        assert_eq!(serialized(&b), before);
        assert!(!stats.changed());
    }

    /// The levels compose: each one does everything the one below it does.
    #[test]
    fn the_levels_are_cumulative_and_grow_monotonically() {
        let mut sizes = Vec::new();
        for level in Compact::ALL {
            let (after, _) = apply_to(level);
            sizes.push(serialized(&after).len());
        }
        for pair in sizes.windows(2) {
            assert!(pair[1] <= pair[0], "each level must not grow the body: {sizes:?}");
        }
        assert!(
            sizes[0] > sizes[4],
            "`all` must actually be smaller than none: {sizes:?}"
        );
        assert_eq!(
            sizes[2], sizes[3],
            "this fixture only calls Bash, so layer D has nothing to stub: {sizes:?}"
        );
        assert_eq!(
            sizes[3], sizes[4],
            "and it has no banners either, so all adds nothing over writes: {sizes:?}"
        );

        // And the flags agree with the names.
        assert!(!Compact::None.does_tools() && !Compact::None.does_thinking() && !Compact::None.does_banners());
        assert!(Compact::Tools.does_tools() && !Compact::Tools.does_thinking());
        assert!(Compact::ToolsThinking.does_thinking() && !Compact::ToolsThinking.does_writes());
        assert!(Compact::Writes.does_writes() && !Compact::Writes.does_banners());
        assert!(Compact::All.does_banners());
    }

    // ---- Layer D ----------------------------------------------------------

    /// The real-world shape layer D exists for: a `Write` whose `tool_result`
    /// is a four-word confirmation while the file body sits in the call's own
    /// `input`. Layer A reaches those results — it has no size gate — but what
    /// it finds there is 25 bytes, while the payload it cannot reach is 5 KB.
    /// That asymmetry is why this layer is not redundant with it.
    fn write_body() -> serde_json::Value {
        let mut messages = Vec::new();
        for i in 0..TURNS {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("call_{i:02}"),
                    "content": "File created successfully",
                }],
            }));
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "t".repeat(200 + i), "signature": format!("sig{i:02}")},
                    {
                        "type": "tool_use",
                        "id": format!("call_{i:02}"),
                        "name": "Write",
                        // Sizes differ per turn, so the stub's byte count is
                        // checkable against a specific index.
                        "input": {"file_path": format!("/tmp/f{i:02}.rs"), "content": "x".repeat(5000 + i)},
                    },
                ],
            }));
        }
        json!({"model": "claude-opus-5", "max_tokens": 4096, "messages": messages})
    }

    /// A body of `n` tool turns, each assistant turn built by `assistant(i)`.
    ///
    /// For the fixtures that are about one layer's window rather than its
    /// effect: `KEEP_TURNS` turns is the threshold, so a test that wants
    /// anything elided at all needs more than that.
    fn turns(n: usize, assistant: impl Fn(usize) -> serde_json::Value) -> serde_json::Value {
        let mut messages = Vec::new();
        for i in 0..n {
            messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": format!("call_{i:02}"), "content": "ok"}],
            }));
            messages.push(json!({"role": "assistant", "content": [assistant(i)]}));
        }
        json!({"model": "claude-opus-5", "max_tokens": 4096, "messages": messages})
    }

    /// Every `tool_use` `input`, in order.
    fn write_inputs(body: &serde_json::Value) -> Vec<&serde_json::Value> {
        messages(body)
            .iter()
            .filter_map(blocks)
            .flatten()
            .filter(|b| block_type(b) == Some("tool_use"))
            .filter_map(|b| b.get("input"))
            .collect()
    }

    #[test]
    fn layer_d_stubs_the_file_bodies_of_old_write_calls() {
        let mut b = write_body();
        let stats = apply(&mut b, Compact::Writes);

        assert_eq!(stats.writes_elided, TURNS - KEEP_TURNS);
        assert_eq!(
            stats.tool_results_elided,
            TURNS - KEEP_TURNS,
            "layer A reaches the confirmations too — it has no size gate"
        );

        // …but what A finds there is nothing, which is the point. Measured
        // rather than asserted in prose: A alone leaves the bodies behind.
        let mut with_a_only = write_body();
        let _ = apply(&mut with_a_only, Compact::Tools);
        let saved = serialized(&with_a_only).len() - serialized(&b).len();
        assert!(
            saved > 19_000,
            "the file bodies are the bulk and only D removes them; D saved {saved} bytes"
        );

        let inputs = write_inputs(&b);
        assert_eq!(inputs.len(), TURNS, "no call may be removed");

        // The stubbed ones keep everything but the body, including the path
        // that makes the stub worth reading.
        let first = inputs[0];
        assert_eq!(first["file_path"], "/tmp/f00.rs");
        let content = first["content"].as_str().expect("content must stay a string");
        assert!(content.starts_with(WRITE_ELIDED_PREFIX), "{content}");
        assert!(
            content.contains("5002 bytes"),
            "the count is of the serialized string it replaced: {content}"
        );
        assert!(
            !content.contains("5000 bytes"),
            "the count must include the JSON quotes, not the raw length: {content}"
        );

        // …and the kept ones are byte-identical.
        let before = write_body();
        let originals = write_inputs(&before);
        for (i, kept) in inputs.iter().enumerate().skip(TURNS - KEEP_TURNS) {
            assert_eq!(
                serialized(kept),
                serialized(originals[i]),
                "the kept writes must be byte-identical: {i}"
            );
        }
    }

    /// Layer D is the mirror of A and must not run before its level: an
    /// operator who chose `tools_thinking` did not ask for write bodies to go.
    #[test]
    fn layer_d_only_runs_at_its_own_level() {
        for level in [Compact::None, Compact::Tools, Compact::ToolsThinking] {
            let mut b = write_body();
            let stats = apply(&mut b, level);
            assert_eq!(stats.writes_elided, 0, "{level:?} stubbed a write");
            let first = write_inputs(&b)[0];
            assert_eq!(
                first["content"].as_str().expect("string").len(),
                5000,
                "{level:?} touched a file body"
            );
        }
    }

    /// Upstream validates a re-sent `tool_use.input` against the tool's schema
    /// on every turn. Stubbing a VALUE cannot fail a `type: string`; removing
    /// a KEY fails `required`. This is the invariant that keeps the layer legal.
    #[test]
    fn layer_d_leaves_every_input_key_in_place() {
        let before = write_body();
        let mut after = before.clone();
        let _ = apply(&mut after, Compact::Writes);
        for (b, a) in write_inputs(&before).iter().zip(write_inputs(&after)) {
            let b = b.as_object().expect("object");
            let a = a.as_object().expect("object");
            let keys: Vec<&String> = b.keys().collect();
            assert_eq!(keys, a.keys().collect::<Vec<_>>(), "a key was removed or added");
            for (name, value) in a {
                assert_eq!(
                    b[name].is_string(),
                    value.is_string(),
                    "{name} changed type — a schema check would fail"
                );
            }
        }
    }

    /// A stub costs bytes. Below the threshold, replacing would make the body
    /// BIGGER — a small `Edit` is two short strings and a path.
    ///
    /// Sized past `KEEP_TURNS` so the short strings are genuinely inside the
    /// window: a fixture smaller than the window would pass this vacuously.
    #[test]
    fn layer_d_leaves_short_strings_alone() {
        let mut b = turns(TURNS + 2, |_| {
            json!({
                "type": "tool_use", "id": "call", "name": "Edit",
                "input": {"file_path": "/tmp/a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"},
            })
        });
        let before = write_inputs(&b).into_iter().map(serialized).collect::<Vec<_>>();
        let stats = apply(&mut b, Compact::Writes);
        assert_eq!(stats.writes_elided, 0);
        // The inputs specifically, not the whole body: layer A runs at this
        // level too and elides the `ok` results, which would mask a write stub.
        let after = write_inputs(&b).into_iter().map(serialized).collect::<Vec<_>>();
        assert_eq!(before, after, "a small edit must not grow");
    }

    /// The gate is the tool NAME, not the shape of `input`. A `Bash` call with
    /// a long command is work the model may still need to read.
    #[test]
    fn layer_d_does_not_touch_tools_that_are_not_file_writes() {
        let command = "echo".to_owned() + &" a".repeat(4000);
        let mut b = turns(TURNS + 2, |_| {
            json!({
                "type": "tool_use", "id": "call", "name": "Bash",
                "input": {"command": command, "description": "long"},
            })
        });
        let stats = apply(&mut b, Compact::Writes);
        assert_eq!(stats.writes_elided, 0);
        assert!(
            write_inputs(&b)[0]["command"]
                .as_str()
                .expect("string")
                .contains(" a a")
        );
    }

    /// The same rule as every other layer: a retry, or a second route change
    /// on one request, re-runs the pass over its own output. Without the
    /// marker the stub would be re-stubbed and the count would become the
    /// stub's own length on every pass.
    #[test]
    fn layer_d_is_idempotent_and_reports_the_original_size() {
        let mut once = write_body();
        let _ = apply(&mut once, Compact::Writes);
        let mut twice = once.clone();
        let stats = apply(&mut twice, Compact::Writes);
        assert_eq!(serialized(&twice), serialized(&once), "idempotent");
        assert_eq!(stats.writes_elided, 0, "and reports nothing to do");

        // The count is of what it replaced, not of the stub.
        let stub = write_inputs(&twice)[0]["content"].as_str().expect("string");
        assert!(stub.contains("5002 bytes"), "{stub}");
        assert!(!stub.contains("bytes] bytes"), "the stub consumed itself: {stub}");
    }

    /// Nesting: `MultiEdit` keeps its edits in an array, and a stub has to be
    /// found there too or the layer silently skips a whole tool.
    #[test]
    fn layer_d_reaches_strings_nested_in_arrays() {
        let mut b = turns(TURNS + 2, |_| {
            json!({
                "type": "tool_use", "id": "call", "name": "MultiEdit",
                "input": {"file_path": "/tmp/a.rs", "edits": [
                    {"old_string": "a".repeat(300), "new_string": "b".repeat(300)},
                    {"old_string": "c".repeat(10), "new_string": "d".repeat(10)},
                ]},
            })
        });
        let stats = apply(&mut b, Compact::Writes);
        assert_eq!(
            stats.writes_elided,
            2 * (TURNS + 2 - KEEP_TURNS),
            "two long strings in each turn outside the window, and no others"
        );
        let edits = write_inputs(&b)[0]["edits"].as_array().expect("array");
        assert!(
            edits[0]["old_string"]
                .as_str()
                .expect("s")
                .starts_with(WRITE_ELIDED_PREFIX)
        );
        assert!(
            edits[0]["new_string"]
                .as_str()
                .expect("s")
                .starts_with(WRITE_ELIDED_PREFIX)
        );
        assert_eq!(edits[1]["old_string"], "c".repeat(10), "a short one is left alone");
        assert_eq!(edits[1]["new_string"], "d".repeat(10), "a short one is left alone");
    }

    /// The window is counted over turns that HAVE a write call, so "keep 6"
    /// keeps six writes rather than six assistant messages — the same rule
    /// layers A and B follow.
    #[test]
    fn layer_d_counts_its_window_over_qualifying_turns() {
        // Ten writing turns; the newest two become `Bash`. The window keeps
        // the six newest WRITES, which are now turns 2..7 — not the six newest
        // messages, which would have reached back to turn 2 as well only by
        // coincidence of the interleaving. What it must not do is keep the two
        // Bash turns in place of two writes and stop at turn 4.
        let mut b = turns(TURNS, |i| {
            if i >= TURNS - 2 {
                return json!({"type": "tool_use", "id": format!("call_{i:02}"), "name": "Bash",
                              "input": {"command": "ls"}});
            }
            json!({
                "type": "tool_use", "id": format!("call_{i:02}"), "name": "Write",
                "input": {"file_path": format!("/tmp/f{i:02}.rs"), "content": "x".repeat(5000)},
            })
        });
        let stats = apply(&mut b, Compact::Writes);
        assert_eq!(
            stats.writes_elided, 2,
            "eight writes remain and six are kept, so exactly two are stubbed"
        );
        let inputs = write_inputs(&b);
        for (i, input) in inputs.iter().enumerate() {
            // The prefix, not merely "has a content field": a kept write has
            // one too, and a Bash input has none at all.
            let stubbed = input["content"]
                .as_str()
                .is_some_and(|s| s.starts_with(WRITE_ELIDED_PREFIX));
            assert_eq!(
                stubbed,
                i < 2,
                "turn {i} stub state — the two non-writing turns must not shorten the window"
            );
        }
    }

    /// The pairing invariant, re-checked with layer D in play — a `tool_use`
    /// and its `tool_result` are a pair, and this layer edits the OTHER half
    /// of the pair from layer A.
    #[test]
    fn layer_d_leaves_the_pairing_alone() {
        let (before_uses, before_results) = pair_ids(&write_body());
        assert_eq!(before_uses, before_results, "the fixture is balanced to begin with");

        for level in Compact::ALL {
            let mut b = write_body();
            let _ = apply(&mut b, level);
            let (uses, results) = pair_ids(&b);
            assert_eq!(uses, before_uses, "{level:?} changed the tool_use ids");
            assert_eq!(results, before_results, "{level:?} changed the tool_result ids");
            assert_eq!(uses, results, "{level:?} unbalanced the pairing");
        }
    }
}
