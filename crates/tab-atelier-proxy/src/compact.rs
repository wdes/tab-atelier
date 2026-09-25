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
//! A provider that is not Anthropic was assumed to have no prompt cache at
//! all, which was the original justification for compacting here: if the cache
//! is gone the moment routing sends a call elsewhere, shortening the history
//! costs nothing. **That assumption is wrong.** Measured on the live `DeepSeek`
//! hop, the auto-mode classifier — a request this pass deliberately never
//! touches — reads back 99.8% of its prompt from cache, and conversations sit
//! at 85–98%. So a non-Anthropic hop does cache, and rewriting the middle of
//! `messages` there invalidates the prefix exactly as it would on Anthropic.
//!
//! What follows from that, and what is deliberately not yet acted on: this
//! pass cannot see which `cache_control` breakpoints a body carries, so it
//! cannot tell a cold conversation from a warm one. It compacts both. The
//! saving is real (the bytes are old turns, priced at cache-hit rates), but it
//! is smaller than it looks and it is paid for in invalidated prefixes. Making
//! the pass idle-gated — compact only when the conversation is cold enough
//! that no warm prefix exists to break — is the fix, and it needs a per-
//! conversation signal this process does not currently keep.
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
//! # Why write payloads are no longer elided
//!
//! Layer D used to replace the long string arguments of old `Write` and `Edit`
//! calls — the model's own authored text — with a stub. It is gone, and the
//! reason is not that it saved too little.
//!
//! A stub standing where file content was is text the model can copy. On
//! 2026-09-16 an agent did exactly that: it read a stubbed `Write` in its own
//! history, took the stub for the file, and wrote the marker into
//! `046-boot-text.sh`, destroying `policies.json` and four memory files. The
//! same mechanism put markers into pull-request bodies. Nothing on the response
//! path can catch it after the fact — the relay streams the answer back
//! untouched and never sees the arguments the model emits.
//!
//! Eliding a `tool_result` is a different thing. Those carry command output,
//! which nothing renders back as a file body, and the model is told the content
//! is gone. Layer D replaced text the model had *authored*, so a stub there read
//! as something it had authored too.
//!
//! What it cost: measured over 150 live requests it was 11% of everything
//! compaction removed (range 7–17%), worth roughly $59/month — the bytes are
//! old turns, so they are priced at cache-hit rates rather than the miss rate
//! that dominates the bill. There was no safe subset to keep either: 93% of
//! those bytes sat in payloads under 10 KB, so the saving was in volume rather
//! than in a few outliers that could have been exempted.
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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How many trailing turns each layer leaves alone.
///
/// Six because that is roughly the span the model is still reasoning over —
/// enough that nothing in flight gets elided from under it, small enough that
/// a long session sheds most of its bulk.
pub const KEEP_TURNS: usize = 6;

/// How many qualifying messages may sit past the window before the boundary moves.
///
/// The window is counted back from the end, so with the plain rule it slides by
/// one message per turn — and each slide rewrites a message the client has
/// already sent. A provider whose cache matches a **strict prefix** answers that
/// by re-processing everything from the change to the end of the prompt, so the
/// price of eliding one turn is the whole retained window, paid again on every
/// turn. That is not hypothetical: the fleet's non-Anthropic traffic re-bills
/// ~6,400 tokens a turn at 391,000 of context, flat across 25,000 to 744,000,
/// which is about the six turns this window keeps. Anthropic's cache absorbs the
/// same rewrite without re-billing, which is why the effect showed up on one
/// model and not the others.
///
/// Eliding a batch at a time does the same elision for a fraction of the
/// invalidation: the boundary moves once every this many turns instead of every
/// turn. The cost is that up to `ELIDE_BATCH - 1` extra turns ride inside the
/// window, so it keeps between `KEEP_TURNS` and `KEEP_TURNS + ELIDE_BATCH - 1`
/// turns. Those extra turns are served from cache, which is the cheap way to
/// carry them; moving the boundary continuously is what pays for them over and
/// over.
///
/// Four rather than more, because the turns held back are *not* elided and so
/// are carried at full size — this bounds that at three turns while already
/// cutting the re-billed span by about two and a half.
pub const ELIDE_BATCH: usize = 4;

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
    /// Layer C on top of the rest: stale injected system notices go too.
    ///
    /// That means both the `<total_tokens>` banners, of which only the newest
    /// survives, and the harness's other `role: "system"` messages, which are
    /// transient by nature and the newest [`KEEP_TURNS`] of which are kept.
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
            Self::All => "Remove old tool results, thinking and system notices",
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

    /// Whether this level drops stale injected system notices.
    #[must_use]
    pub const fn does_notices(self) -> bool {
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
    /// Tool results left alone because a stub would have been longer than the
    /// result it replaced — see [`MIN_TOOL_RESULT_BYTES`].
    pub tool_results_kept_small: usize,
    /// Tool results left alone because the stale region they sit in was under
    /// [`ELIDE_ABOVE_BYTES`] — see the note there. Nonzero means the pass found
    /// work and declined it, which is a different answer from "nothing to do"
    /// and the one an operator debugging an un-shrunk transcript is asking for.
    ///
    /// Deliberately not part of [`Stats::changed`]: declining is not a change.
    pub tool_results_kept_under_budget: usize,
    pub thinking_dropped: usize,
    pub notices_dropped: usize,
}

impl Stats {
    /// Whether the pass changed anything worth logging.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.tool_results_elided > 0 || self.thinking_dropped > 0 || self.notices_dropped > 0
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

/// The marker every stub begins with, followed by the byte count of the content
/// it replaced, a `; `, and the call that produced it — `[elided:90002B; Read …]`.
///
/// Kept as short as it can be while still telling the model two things at the
/// point it reads the stub, which are the two things it needs to decide whether
/// to look at the result again:
///
/// 1. *something was here, and how much* — so a stub is never read as an empty
///    result, which a model answers by calling the tool again.
/// 2. *which call this was* — so it can tell a result it has already reasoned
///    over from one it has never seen. Without this the stub carries no more
///    information than a fresh call would, and re-reading is the only way to
///    find out, which is the loop the pass was meant to prevent.
///
/// The `tool_use_id` is deliberately *not* in the text: it is 49 bytes, and it is
/// already the sibling field of the block being annotated — in one captured body
/// 803 of 804 stubs repeated a value sitting right next to them. The prose went
/// the same way. What replaced it is the call's *target*, which is the part the
/// model actually matches against its own history; the id could not serve there,
/// because the model never saw the id when it made the call.
///
/// The previous text was
/// `[tool result elided by tab-atelier-proxy: N bytes; tool_use_id=…]` — 102
/// bytes, where the body of this one is typically 25-30. See [`stub`] for the
/// text and [`ELIDE_ABOVE_BYTES`] for when it is used at all.
const ELIDED_PREFIX: &str = "[elided:";

/// The marker this replaced.
///
/// Still recognised, so that a conversation already in flight when the format
/// changed is not elided a second time: a stub inside a stub tells the model
/// less than the original stub did, and the byte count would then shrink on
/// every pass instead of staying the size of the result it replaced.
///
/// It can be deleted once no live conversation predates the change — stubs only
/// exist inside bodies, so they age out with the conversations holding them.
const ELIDED_PREFIX_LEGACY: &str = "[tool result elided by tab-atelier-proxy: ";

/// The shortest `tool_result` worth replacing with a stub.
///
/// A tool result is evidence, and the short ones are the evidence that matters
/// most: "The file /src/lib.rs has been updated." is the only record in the
/// conversation that the call succeeded. Replacing it saves a handful of bytes
/// and leaves the model unable to tell "it worked" from "that call never
/// happened" — and an agent that cannot tell those apart retries work it has
/// already done.
///
/// It was previously implicit. The rule was "only when the stub is strictly
/// shorter than the result", which with a 102-byte stub meant "over 102 bytes"
/// by accident. Stating it matters because the marker is now 14 bytes: without
/// this floor the same rule would have started eliding acknowledgements, and
/// nothing in the design would have said that was wrong.
const MIN_TOOL_RESULT_BYTES: u64 = 200;

/// How much stale `tool_result` content a request must carry before layer A
/// does anything at all.
///
/// Elision exists to keep a request inside the model's context window, and that
/// is a problem only above a certain size: most of the bytes in a long session
/// are machine payload, so on a transcript that has grown into the megabytes the
/// pass pays for itself many times over. On a request that already fits it pays
/// for nothing the provider was charging for, and costs the model the contents
/// of calls it is still working with — plus, on any hop with a prompt cache, an
/// invalidated prefix (see the module docs).
///
/// That cost is not theoretical, and it is why this floor exists. On 2026-09-25 a
/// `catbus-agent` tab spent 200 rounds and 2.55M input tokens re-reading files
/// whose results this pass had replaced with stubs. The request was never over
/// any budget, so the elision bought no context — while the stub left nothing to
/// reason about, and the model had no way to tell a fresh read from a recycled
/// one. It could only read again, so it did, until the round cap stopped it.
///
/// Measured on the stale region only — the bytes this pass would actually
/// remove — because that is the tightest available criterion: "is there enough
/// old tool output here to be worth trimming?" A body with less is returned
/// untouched, which is also the cheapest possible answer.
pub const ELIDE_ABOVE_BYTES: u64 = 256 * 1024;

/// The keys a tool call names its target with, most specific first.
const TARGET_KEYS: [&str; 6] = ["file_path", "path", "pattern", "command", "query", "name"];

/// How many characters of a call's target survive into the stub.
const TARGET_KEEP: usize = 48;

/// The stub that replaces an elided `tool_result`'s content.
///
/// The block, its id and its position all stay. That is the point: the model is
/// told *something was there*, rather than being shown an empty result and
/// concluding the tool returned nothing. The id is not repeated in the text
/// because it is already the sibling field of the block this replaces.
///
/// What is repeated is *what was called*. The byte count alone says something
/// was here and how big it was, which leaves the model to work out whether the
/// thing it needs is inside it — and the safe answer to that question, when the
/// task depends on it, is to read it again. Naming the call answers a different
/// question, the one the model actually has: "have I already read this file this
/// conversation?" With the name in the stub it can see that it has, and use the
/// reasoning it did the first time. Without it, re-reading is the only way to
/// find out, which is the loop this pass was supposed to prevent.
///
/// The target, not the `tool_use_id`: the id is 49 bytes and already the sibling
/// field of the block being annotated, while the target is the string the model
/// wrote in the call and can match against its own history. It stays a pure
/// function of the body — see the module docs on determinism — because it is read
/// out of the `tool_use` the stub answers.
///
/// What the stub deliberately does *not* do is tell the model what to do about it
/// — no "re-read a narrower range", no "this content is still in your context".
/// Both would be claims about behaviour this side cannot promise, and the second
/// is actually false: a re-read's result is the newest in the body, so it sits
/// inside the keep window and comes back in full. The stub's job is to report what
/// happened. Guidance belongs where it can be checked against real outcomes — the
/// client's loop guard, which sees the calls and the results both.
fn stub(byte_count: u64, provenance: Option<&str>) -> String {
    provenance.map_or_else(
        || format!("{ELIDED_PREFIX}{byte_count}B]"),
        |what| format!("{ELIDED_PREFIX}{byte_count}B; {what}]"),
    )
}

/// Map every `tool_use` in `messages` from its id to a short "what was called".
///
/// Built from the same slice the stubs are written into: a `tool_result` can only
/// be answered by a `tool_use` that came before it, so a call whose result is in
/// the stale region is itself in there.
fn call_targets(messages: &[serde_json::Value]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for message in messages {
        let Some(list) = blocks(message) else { continue };
        for block in list {
            if block_type(block) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let name = block.get("name").and_then(serde_json::Value::as_str).unwrap_or("tool");
            out.insert(id.to_owned(), describe_call(name, block.get("input")));
        }
    }
    out
}

/// "`Read /src/lib.rs`", or just "`Read`" when the call names no target.
fn describe_call(name: &str, input: Option<&serde_json::Value>) -> String {
    let target = input.and_then(|input| {
        TARGET_KEYS
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
    });
    match target.map(first_line_clipped) {
        Some(target) if !target.is_empty() => format!("{name} {target}"),
        _ => name.to_owned(),
    }
}

/// The first line of a target, clipped so a stub cannot inherit a whole prompt.
fn first_line_clipped(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= TARGET_KEEP {
        return line.to_owned();
    }
    let clipped: String = line.chars().take(TARGET_KEEP).collect();
    format!("{clipped}…")
}

/// Why a `tool_result` is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// It carries an error, and the error text is what makes it useful.
    Error,
    /// No `tool_use_id`, so a stub would point at nothing.
    Unbound,
    /// Already a stub from an earlier pass.
    Stubbed,
    /// Shorter than [`MIN_TOOL_RESULT_BYTES`], or with no content at all.
    Small,
}

/// Whether this `tool_result` should be stubbed, and how much it would save.
///
/// The caller has already checked the block is a `tool_result`; every other
/// reason to leave it alone is here, so the counting pass and the stub text can
/// never disagree about what is eligible.
///
/// An elided error is the one elision class where the loss is semantic rather
/// than bulk — "use the Grep tool instead" is not something the model can
/// re-derive from a byte count. A few KB is a cheap price for never doing that.
fn elidable(block: &serde_json::Value) -> Result<u64, Keep> {
    if block
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(Keep::Error);
    }
    // A result with no id is not bound to a call, so a stub would leave content
    // pointing at nothing. Not the shape Claude Code sends, and cheap to refuse.
    if block.get("tool_use_id").and_then(serde_json::Value::as_str).is_none() {
        return Err(Keep::Unbound);
    }
    let Some(content) = block.get("content") else {
        return Err(Keep::Small);
    };
    // Already a stub from a previous pass: leaving it alone keeps the original
    // byte count in the message and makes the pass idempotent.
    if already_elided(content) {
        return Err(Keep::Stubbed);
    }
    // The floor decides, not the stub length. See `MIN_TOOL_RESULT_BYTES`.
    let bytes = bytes_of(content);
    if bytes < MIN_TOOL_RESULT_BYTES {
        return Err(Keep::Small);
    }
    Ok(bytes)
}

/// Whether this content is already a stub from an earlier pass.
fn already_elided(content: &serde_json::Value) -> bool {
    content
        .as_str()
        .is_some_and(|s| s.starts_with(ELIDED_PREFIX) || s.starts_with(ELIDED_PREFIX_LEGACY))
}

/// The oldest message index still inside the trailing window.
///
/// Counted from the end over messages that `qualifies` — that is, over the
/// turns this layer actually acts on. Counting *all* messages instead would
/// make the window shrink whenever a turn happened to contain nothing of the
/// relevant kind, which is how a "keep 6" rule quietly keeps 3.
///
/// The boundary is quantised to whole `ELIDE_BATCH`es, so it moves once every
/// few turns rather than continuously. That is the whole point of the constant:
/// a boundary sliding one message per turn re-stubs a message the client has
/// already sent on every turn, and a strict prefix cache re-processes
/// everything behind each of those rewrites.
fn window_start(messages: &[serde_json::Value], keep: usize, qualifies: impl Fn(&serde_json::Value) -> bool) -> usize {
    let total = messages.iter().filter(|m| qualifies(m)).count();
    if total <= keep {
        return 0;
    }
    // Hold back the batches not yet due. With `total - keep` a multiple of the
    // batch this keeps exactly `keep`; otherwise it keeps the remainder as well
    // and the boundary stays put until the next whole batch has accumulated.
    let keep = keep + (total - keep) % ELIDE_BATCH;
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
///
/// Nothing happens at all unless the stale region holds more than
/// [`ELIDE_ABOVE_BYTES`] of elidable content; see that constant for why the
/// floor is there.
fn elide_tool_results(messages: &mut [serde_json::Value], stats: &mut Stats) {
    let start = window_start(messages, KEEP_TURNS, |m| has_block(m, "tool_result"));
    // Classify the region first, decide, then rewrite. Rewriting as we walked
    // would mean the budget could not be consulted before the first stub was
    // written — it is a property of the whole region, so it cannot be known
    // from inside it.
    let mut candidates: Vec<(usize, usize, u64)> = Vec::new();
    for (index, message) in messages[..start].iter().enumerate() {
        let Some(list) = blocks(message) else { continue };
        for (slot, block) in list.iter().enumerate() {
            if block_type(block) != Some("tool_result") {
                continue;
            }
            match elidable(block) {
                Ok(bytes) => candidates.push((index, slot, bytes)),
                Err(Keep::Error) => stats.tool_results_kept_for_error += 1,
                Err(Keep::Small) => stats.tool_results_kept_small += 1,
                // Nothing was saved, so nothing needs saying: `Unbound` is not
                // the shape Claude Code sends, and `Stubbed` is this pass having
                // already run on this body.
                Err(Keep::Unbound | Keep::Stubbed) => {}
            }
        }
    }
    let stale_bytes: u64 = candidates.iter().map(|(_, _, bytes)| bytes).sum();
    if stale_bytes < ELIDE_ABOVE_BYTES {
        stats.tool_results_kept_under_budget = candidates.len();
        return;
    }
    let targets = call_targets(&messages[..start]);
    for (index, slot, bytes) in candidates {
        let Some(block) = blocks_mut(&mut messages[index]).and_then(|list| list.get_mut(slot)) else {
            continue;
        };
        let provenance = block
            .get("tool_use_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| targets.get(id))
            .map(String::as_str);
        block["content"] = serde_json::Value::String(stub(bytes, provenance));
        stats.tool_results_elided += 1;
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

/// Whether a message is one of the re-injected system notices this pass drops.
///
/// `system` is not a role the Messages API defines, so a message carrying it is
/// by construction something the client spliced in — the harness's own prompt
/// travels in the top-level `system` field instead. Nothing the model authored
/// can be lost by dropping one, which is what makes this safe to do by role
/// rather than by matching each notice's wording.
fn is_notice(message: &serde_json::Value) -> bool {
    message.get("role").and_then(serde_json::Value::as_str) == Some("system")
}

/// Layer C — drop stale injected system notices, keeping the newest few.
///
/// A wider door than [`is_banner`], which keeps exactly one: a banner is a live
/// number the model reads to judge its own budget, so only the newest matters,
/// while a notice is an event — a tool that errored, a note that the user
/// replied — that stays useful for a turn or two before it is only noise. The
/// window is what separates them; both are removed past it.
///
/// This is the largest class no other layer touches, because none of them look
/// at `role` at all.
fn drop_notices(messages: &mut Vec<serde_json::Value>, stats: &mut Stats) {
    // `window_start` gives the OLDEST notice to keep; everything before it goes.
    // Walked backwards so each removal leaves the indices below it untouched —
    // a forward loop would shift the next notice down by one and skip it.
    let mut index = window_start(messages, KEEP_TURNS, is_notice);
    while index > 0 {
        index -= 1;
        if is_notice(&messages[index]) {
            messages.remove(index);
            stats.notices_dropped += 1;
        }
    }
}

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
            stats.notices_dropped += 1;
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
    if level.does_notices() {
        drop_banners(messages, &mut stats);
        drop_notices(messages, &mut stats);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// How many turns the fixtures below build.
    const TURNS: usize = 10;

    /// How big each `tool_result` payload is in the fixture.
    ///
    /// Chosen to clear [`ELIDE_ABOVE_BYTES`] even after a test replaces one of the
    /// stale results with a short acknowledgement: the remaining candidates are
    /// 270 KB between them, over the 256 KiB floor. A fixture that did not clear
    /// the floor would make every layer-A assertion below vacuous — the pass would
    /// decline the body and `tool_results_elided` would be 0 for a reason the test
    /// never meant to assert.
    const PAYLOAD: usize = 90_000;

    /// A body in the shape Claude Code sends: a system prompt, tool schemas,
    /// and alternating turns — a user turn carrying a `tool_result`, an
    /// assistant turn carrying `thinking` and the `tool_use` it answers.
    ///
    /// Every part the pass must not touch is present, because "we did not
    /// change it" is only worth asserting when there was something there to
    /// change.
    fn body() -> serde_json::Value {
        body_sized(PAYLOAD)
    }

    /// The same body, too small for layer A to touch. See [`ELIDE_ABOVE_BYTES`].
    fn small_body() -> serde_json::Value {
        body_sized(1000)
    }

    fn body_sized(payload: usize) -> serde_json::Value {
        let mut messages = Vec::new();
        for i in 0..TURNS {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("call_{i:02}"),
                    // Distinct sizes, so the stub's byte count is checkable.
                    "content": "r".repeat(payload + i),
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

    /// The byte count a stub should carry for turn `i` of [`body`]: the payload
    /// plus the two quotes `serde_json` wraps it in.
    fn stubbed_bytes(i: usize) -> u64 {
        (PAYLOAD + i + 2) as u64
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

        // The elided ones carry the stub: the byte count of what they replaced,
        // and nothing else. The id is asserted separately, as the sibling field
        // it already is — repeating it in the text cost 49 bytes a stub and told
        // the model nothing it could not read one field over.
        let first = results[0];
        assert_eq!(first["tool_use_id"], "call_00");
        let text = first["content"].as_str().expect("content became a string");
        assert_eq!(text, format!("[elided:{}B; Bash ls]", stubbed_bytes(0)));
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

    /// The real property: the boundary advances a whole batch at a time.
    ///
    /// The window is counted back from the end, so the plain rule advances it by
    /// one message per turn, and each advance stubs a message the client has
    /// already sent. A provider whose cache matches a strict prefix re-processes
    /// everything from that message to the end of the prompt, so the cost is the
    /// retained window — paid again on every single turn.
    ///
    /// "Never rewrite an already-sent message" is not the property to assert,
    /// because it is impossible while eliding anything at all: a message is sent
    /// the moment it appears, so any elision of it is a rewrite of it. The
    /// achievable property is that the boundary moves once per batch, which is
    /// what divides the cost by `ELIDE_BATCH`.
    #[test]
    fn the_elision_boundary_advances_a_batch_at_a_time() {
        /// Comfortably past `KEEP_TURNS`, so the boundary has room to move.
        const GROWN: usize = 24;

        let mut body = body();
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut rewrote_on: Vec<usize> = Vec::new();

        for turn in 0..GROWN {
            let _ = apply(&mut body, Compact::Tools);
            let now: Vec<Vec<u8>> = messages(&body).iter().map(serialized).collect();

            if sent
                .iter()
                .enumerate()
                .any(|(i, was)| now.get(i).is_some_and(|message| message != was))
            {
                rewrote_on.push(turn);
            }
            sent = now;
            push_turn(&mut body, turn);
        }

        assert!(
            !rewrote_on.is_empty(),
            "nothing the client had already sent was rewritten over {GROWN} turns, so this test \
             asserted nothing"
        );
        for pair in rewrote_on.windows(2) {
            let apart = pair[1] - pair[0];
            assert!(
                apart >= ELIDE_BATCH - 1,
                "the boundary advanced twice within {apart} turn(s) — on turns {rewrote_on:?}. \
                 Elision is meant to move a batch of {ELIDE_BATCH} messages at once; moving it per \
                 turn re-processes the retained window on every turn, which is what a strict \
                 prefix cache charges for."
            );
        }
    }

    /// One more turn in the shape [`body`] builds: a user turn carrying a
    /// `tool_result`, then the assistant turn that answers it.
    fn push_turn(body: &mut serde_json::Value, i: usize) {
        let list = body["messages"].as_array_mut().expect("messages");
        list.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": format!("call_{i:02}"),
                "content": "r".repeat(PAYLOAD + i),
            }],
        }));
        list.push(json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "t".repeat(200 + i), "signature": format!("sig{i:02}")},
                {"type": "tool_use", "id": format!("call_{i:02}"), "name": "Bash", "input": {"command": "ls"}},
            ],
        }));
    }

    /// A body with too little stale tool output is forwarded untouched, whole.
    ///
    /// Layer A is a context-window measure, and that is only a problem above a
    /// certain size. Below [`ELIDE_ABOVE_BYTES`] the pass would buy back no
    /// context the provider was charging for, while still taking away the model's
    /// sight of results it is mid-way through using. So it declines the body —
    /// and declines it byte for byte, rather than partially, so the request the
    /// provider sees is the one the client sent.
    ///
    /// The regression this guards: on 2026-09-25 a `catbus-agent` tab spent 200
    /// rounds and 2.55M input tokens with every tool result stubbed, on a request
    /// that was never over any budget.
    #[test]
    fn a_small_stale_region_is_left_completely_alone() {
        let mut b = small_body();
        let before = serialized(&b);
        let stats = apply(&mut b, Compact::Tools);

        assert_eq!(serialized(&b), before, "the body must come back byte-identical");
        assert_eq!(stats.tool_results_elided, 0);
        assert_eq!(
            stats.tool_results_kept_under_budget,
            TURNS - KEEP_TURNS,
            "declining is reported, so it is distinguishable from nothing to do"
        );
        assert!(!stats.changed(), "declining is not a change");
    }

    /// The floor gates layer A and nothing else. A small body still loses its
    /// stale thinking: that is a different trade, and one with no such risk —
    /// nothing in the conversation is the model's own reasoning to re-read.
    #[test]
    fn the_floor_does_not_gate_thinking() {
        let mut b = small_body();
        let stats = apply(&mut b, Compact::ToolsThinking);
        assert_eq!(stats.tool_results_elided, 0);
        assert_eq!(stats.thinking_dropped, TURNS - KEEP_TURNS);
        assert!(stats.changed());
    }

    /// The stub names the call that produced it.
    ///
    /// This is the fix for the 2026-09-25 loop. A stub carrying only a byte count
    /// leaves the model unable to tell a result it has already seen from one it
    /// has not, and the cheap way to find out is to call the tool again — which it
    /// did, 200 times, on the same files. The target is what makes the stub
    /// answerable: it is the string the model wrote in the call, so it can match
    /// it against its own history at a glance.
    #[test]
    fn the_stub_names_the_call_that_produced_it() {
        let long_path = format!("/src/{}/unique_tail.rs", "segment/".repeat(8));
        let mut b = body();
        b["messages"][1]["content"][1] = json!({
            "type": "tool_use",
            "id": "call_00",
            "name": "Read",
            "input": {"file_path": long_path},
        });
        let stats = apply(&mut b, Compact::Tools);
        let text = tool_results(&b)[0]["content"].as_str().expect("a stub").to_owned();

        assert_eq!(stats.tool_results_elided, TURNS - KEEP_TURNS);
        assert!(
            text.starts_with(&format!("[elided:{}B; ", stubbed_bytes(0))),
            "the byte count survives the change: {text}"
        );
        assert!(text.contains("Read /src/"), "the call is named: {text}");
        assert!(
            text.ends_with("…]"),
            "a long target is clipped, not carried whole: {text}"
        );
        assert!(
            !text.contains("unique_tail"),
            "the tail of a long target must not be in the stub: {text}"
        );
    }

    /// A call the model made *is* named even when it carries no target string —
    /// the tool is still something to recognise. Only a result whose call cannot
    /// be found at all falls back to the bare count.
    #[test]
    fn a_stub_falls_back_to_the_bare_count_only_when_the_call_is_missing() {
        let mut b = body();
        b["messages"][1]["content"][1] = json!({
            "type": "tool_use",
            "id": "call_00",
            "name": "TodoWrite",
            "input": {"todos": []},
        });
        let _ = apply(&mut b, Compact::Tools);
        let named = tool_results(&b)[0]["content"].as_str().expect("a stub").to_owned();
        assert_eq!(named, format!("[elided:{}B; TodoWrite]", stubbed_bytes(0)));

        // The same result, but the call it belongs to is not in the body — an id
        // no `tool_use` answers. It is still stubbed, because the alternative is
        // forwarding megabytes to save a naming we cannot do.
        let mut b = body();
        b["messages"][1]["content"][1]["id"] = json!("call_99");
        let _ = apply(&mut b, Compact::Tools);
        let bare = tool_results(&b)[0]["content"].as_str().expect("a stub").to_owned();
        assert_eq!(bare, format!("[elided:{}B]", stubbed_bytes(0)));
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
            PAYLOAD + 2,
            "the error's content is intact, not stubbed"
        );
    }

    /// A short acknowledgement is evidence, and is never elided.
    ///
    /// A `Write` or `Edit` acknowledges with a line like "The file /src/lib.rs
    /// has been updated." — 34 bytes. It is the only record in the conversation
    /// that the call succeeded: a model re-reading its own history would find an
    /// opaque stub where "it worked" used to be, and could not tell whether the
    /// edit landed, so it would do the work again.
    ///
    /// This test is why the floor is now a named constant rather than an
    /// accident of the stub's length. It passed for years because the stub was
    /// 102 bytes and the rule was "stub only if strictly shorter"; shrinking the
    /// stub to 14 moved the implicit floor to 14 and this failed immediately.
    #[test]
    fn a_short_acknowledgement_is_never_elided() {
        let ack = "The file /src/lib.rs has been updated.";
        let mut b = body();
        // The oldest result is the one layer A reaches for first.
        b["messages"][0]["content"][0]["content"] = json!(ack);

        let stats = apply(&mut b, Compact::Tools);

        assert_eq!(stats.tool_results_elided, TURNS - KEEP_TURNS - 1);
        assert_eq!(stats.tool_results_kept_small, 1);
        assert_eq!(
            tool_results(&b)[0]["content"],
            ack,
            "a 34-byte acknowledgement is under `MIN_TOOL_RESULT_BYTES` and must survive"
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
        assert_eq!(stats.notices_dropped, 2);
        let banners: Vec<&str> = messages(&b)
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(banners, vec!["<total_tokens>2</total_tokens>"], "the newest survives");

        // A lone `system` message that is not a banner survives — it is inside
        // the notice window, which is why. One notice, and KEEP_TURNS is six.
        let mut b = body();
        let mut msgs = messages(&b).clone();
        msgs.insert(0, json!({"role": "system", "content": "a real instruction"}));
        b["messages"] = json!(msgs);
        let stats = apply(&mut b, Compact::All);
        assert_eq!(stats.notices_dropped, 0);
        assert_eq!(messages(&b)[0]["content"], "a real instruction");
    }

    /// A stale injected notice is dropped, and only the newest `KEEP_TURNS`
    /// are kept. The window is the whole rule for notices — there is no
    /// marker to match, so age is the only thing that can separate one from
    /// another, and the newest is also the least likely to be re-read.
    #[test]
    fn layer_c_drops_only_the_stale_system_notices() {
        let pasted = KEEP_TURNS + 4;
        let mut msgs = Vec::new();
        for i in 0..pasted {
            // Same wording, one in a `user` turn and one as a notice. Only the
            // role decides; the text is a red herring on purpose.
            let text = format!("[SYSTEM NOTIFICATION - NOT USER INPUT] thing {i}");
            msgs.push(json!({"role": "user", "content": text}));
            msgs.push(json!({"role": "system", "content": text}));
        }
        let mut b = json!({"model": "claude-opus-5", "max_tokens": 4096, "messages": msgs});

        let stats = apply(&mut b, Compact::All);
        assert_eq!(stats.notices_dropped, 4);

        let kept: Vec<&str> = messages(&b)
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(kept.len(), KEEP_TURNS);
        assert!(kept.last().expect("kept").ends_with("thing 9"), "newest kept: {kept:?}");
        assert_eq!(
            messages(&b).iter().filter(|m| m["role"] == "user").count(),
            pasted,
            "a user turn that quotes a notice is still a turn"
        );
    }

    /// Every level below `All` leaves notices exactly where they were.
    #[test]
    fn layer_c_is_the_only_level_that_touches_notices() {
        for level in [Compact::None, Compact::Tools, Compact::ToolsThinking] {
            let mut b = body();
            let mut msgs = messages(&b).clone();
            for i in 0..KEEP_TURNS + 3 {
                msgs.insert(0, json!({"role": "system", "content": format!("note {i}")}));
            }
            b["messages"] = json!(msgs);

            let stats = apply(&mut b, level);
            assert_eq!(stats.notices_dropped, 0, "{level:?} dropped a notice");
            assert_eq!(
                messages(&b).iter().filter(|m| m["role"] == "system").count(),
                KEEP_TURNS + 3,
                "{level:?} touched a notice"
            );
        }
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
        assert!(stubs[0].contains(&format!("{}B", stubbed_bytes(0))), "{}", stubs[0]);
        assert!(
            !stubs[0].contains(&format!("{}B", stubs[0].len())),
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
            "and it has no system notices, so all adds nothing over tools_thinking: {sizes:?}"
        );

        // And the flags agree with the names.
        assert!(!Compact::None.does_tools() && !Compact::None.does_thinking() && !Compact::None.does_notices());
        assert!(Compact::Tools.does_tools() && !Compact::Tools.does_thinking());
        assert!(Compact::ToolsThinking.does_thinking() && !Compact::ToolsThinking.does_notices());
        assert!(Compact::All.does_thinking() && Compact::All.does_notices());
    }
}
