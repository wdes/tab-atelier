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
    /// Layer C: B, plus drop stale `<total_tokens>` banners.
    All,
}

impl Compact {
    /// Every level, in order of how much they remove.
    pub const ALL: [Self; 4] = [Self::None, Self::Tools, Self::ToolsThinking, Self::All];

    /// The name as it appears in `providers.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tools => "tools",
            Self::ToolsThinking => "tools_thinking",
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
            Self::All => "Remove old tool results, thinking and banners",
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
        matches!(self, Self::ToolsThinking | Self::All)
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
    pub banners_dropped: usize,
}

impl Stats {
    /// Whether the pass changed anything worth logging.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.tool_results_elided > 0 || self.thinking_dropped > 0 || self.banners_dropped > 0
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
            sizes[0] > sizes[3],
            "`all` must actually be smaller than none: {sizes:?}"
        );
        assert_eq!(
            sizes[2], sizes[3],
            "the fixture has no banners, so all and tools_thinking must agree: {sizes:?}"
        );

        // And the flags agree with the names.
        assert!(!Compact::None.does_tools() && !Compact::None.does_thinking() && !Compact::None.does_banners());
        assert!(Compact::Tools.does_tools() && !Compact::Tools.does_thinking());
        assert!(Compact::ToolsThinking.does_thinking() && !Compact::ToolsThinking.does_banners());
        assert!(Compact::All.does_banners());
    }
}
