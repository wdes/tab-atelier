// SPDX-License-Identifier: MPL-2.0

//! Prompt-cache breakpoints.
//!
//! Until now this agent never sent `cache_control` at all, so every turn
//! re-billed the entire conversation at the miss rate. That is the single
//! largest cost defect it had: on a 366k-token conversation the difference
//! between a cache hit and a miss is 50x per token, and the whole prefix was
//! being re-bought on every request.
//!
//! # How the breakpoints are chosen
//!
//! A breakpoint marks the END of a cacheable prefix: everything up to and
//! including the marked block is stored, and a later request that reproduces
//! those bytes exactly reads them back cheaply. The provider allows **four**
//! breakpoints, and they want to sit at positions that stop moving, because a
//! breakpoint whose surrounding bytes change every turn caches nothing.
//!
//! So the three used here are, in order:
//!
//! 1. the last `system` block — both blocks are static text now, so this
//!    prefix is written once per session and read for the rest of it;
//! 2. the last tool definition — the tool array is resolved once at startup
//!    and never recomputed, so this prefix is stable for the same reason;
//! 3. the last **real** conversation turn, not the synthetic tail this module
//!    appends after it ([`ENV_MARKER`]).
//!
//! The third is the one that has to be right. Marking the final message
//! instead would put the breakpoint on the env turn, which is rebuilt every
//! request with a fresh gate and cwd — so the cached prefix would end one
//! message *before* the stable history and every turn would cache the part
//! that never changes while re-reading the part that does.
//!
//! # Why the live state moved out of `system`
//!
//! The agent used to put cwd and the gate mode in a `system` block, *before*
//! the static instructions. Any `/plan` toggle therefore changed a system
//! block, which invalidated the static block and every message after it — the
//! whole conversation, re-bought to change 20 bytes. The same shape as a tool
//! array that recomputes itself: live state in the prefix.
//!
//! It is now a trailing `user` turn instead, so the prefix is genuinely
//! immutable and only the tail grows.

use serde_json::Value;

/// How many breakpoints the provider accepts. Exceeding it is an error from
/// the API rather than a silent truncation, so this is a hard budget.
pub const MAX_BREAKPOINTS: usize = 4;

/// The tag marking the synthetic turn that carries live state.
///
/// Named so that anything reading a transcript — a human, a log grep, or the
/// judge — can tell a machine-written turn from something the user typed, and
/// so tests can find it without depending on its position.
pub const ENV_MARKER: &str = "<env ";

/// The cache marker, as the API wants it.
fn marker() -> Value {
    serde_json::json!({ "type": "ephemeral" })
}

/// Add `cache_control` to one block or object.
///
/// Returns whether it marked anything, so a caller counting breakpoints is
/// counting what was actually written rather than what it intended to write.
fn mark(block: &mut Value) -> bool {
    if !block.is_object() {
        return false;
    }
    block["cache_control"] = marker();
    true
}

/// Give every message the same content shape, so a turn does not change form
/// between requests.
///
/// A message whose content is a bare string becomes a one-block array. That is
/// a no-op semantically — the API treats `"hi"` and `[{"type":"text","text":
/// "hi"}]` as the same message, and the reference client sends the array form —
/// but it matters for caching, and the reason is subtle enough to be worth
/// spelling out.
///
/// [`mark_breakpoints`] attaches `cache_control` to a *block*, so it has to
/// promote a string to an array to mark it. It only ever marks the last real
/// turn. So without this step the same message arrives as an array on the turn
/// it is last, and as a string on every turn after — a shape change in the
/// middle of the cached prefix, on every single turn, caused by the marking
/// itself. Whether the provider tokenises those two forms identically is not
/// something to bet a 50x price difference on.
///
/// Returns how many messages were rewritten.
pub fn stabilise_shapes(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    for message in messages {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        if let Value::String(text) = content {
            *content = serde_json::json!([{ "type": "text", "text": text }]);
            changed += 1;
        }
    }
    changed
}

/// What a tool that said nothing leaves behind.
const NO_OUTPUT: &str = "(no output)";

/// Drop the content that is not content, and any turn left holding none.
///
/// Anthropic answers a message with nothing in it with `messages.N: all
/// messages must have non-empty content`, so "the content array is non-empty" is
/// not the property that matters. Two shapes were found producing almost that
/// error, from real transcripts rather than from reasoning about the spec:
///
/// * a `thinking` block whose `thinking` was empty — a transcript keeps the
///   block's signature and not always its text;
/// * a turn whose only block was a *full* `thinking` block. Emptiness was the
///   wrong thing to look for: the block is full, it just is not content, and an
///   endpoint that does not implement thinking drops it in transit, so the turn
///   arrives empty. See [`only_deliberation`].
///
/// An empty `tool_result` is filled in rather than dropped, since dropping it
/// would leave the `tool_use` above it unanswered, which the API rejects in
/// turn. Nothing else can be removed without losing something the model asked
/// for, so nothing else is.
///
/// Returns how many blocks and turns were removed or replaced.
pub fn prune_empty_content(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    let mut kept = Vec::with_capacity(messages.len());
    for mut message in messages.drain(..) {
        if let Some(Value::Array(blocks)) = message.get_mut("content") {
            for block in blocks.iter_mut() {
                if block["type"] == "tool_result" && empty_block(block) {
                    block["content"] = Value::String(NO_OUTPUT.to_owned());
                    changed += 1;
                }
            }
            let before = blocks.len();
            blocks.retain(|block| !empty_block(block));
            changed += before - blocks.len();
            // Deliberation on its own is not a turn. Cleared rather than removed
            // here so the check below drops the message in one place, counting it
            // the same way an emptied turn is counted.
            if only_deliberation(blocks) {
                blocks.clear();
                changed += 1;
            }
        }
        if message.get("content").is_none_or(empty_content) {
            changed += 1;
            continue;
        }
        kept.push(message);
    }
    *messages = kept;
    changed
}

/// Whether a message holds nothing but the model's own deliberation.
///
/// A `thinking` block is not content any later turn reads — it is the model's
/// working, kept so a provider that implements thinking can verify it. It also
/// does not survive an endpoint that does not implement it, and every model this
/// relay serves is not Anthropic's, so the block is dropped in transit and the
/// turn arrives with no content at all.
///
/// Found in a real transcript rather than reasoned about: six assistant turns
/// whose only block was thinking, and the API refused exactly those indices
/// (`messages.3`, then `messages.6`) with "all messages must have non-empty
/// content". Emptiness was the wrong thing to look for — the block is full, it
/// just is not content.
fn only_deliberation(blocks: &[Value]) -> bool {
    !blocks.is_empty() && blocks.iter().all(|block| block["type"] == "thinking")
}

/// Whether `block` is a content block with no content in it.
fn empty_block(block: &Value) -> bool {
    match block["type"].as_str() {
        Some("text") => empty_text(block, "text"),
        Some("thinking") => empty_text(block, "thinking"),
        Some("tool_result") => block.get("content").is_none_or(empty_content),
        // Anything unmodelled (a future block type, `redacted_thinking`) is
        // treated as content: its bytes are what the model or the provider
        // asked to see, and this pass only knows about the three shapes it
        // can be sure are empty.
        _ => false,
    }
}

/// Whether a block's string field is missing, empty, or only whitespace.
fn empty_text(block: &Value, field: &str) -> bool {
    block[field].as_str().is_none_or(|text| text.trim().is_empty())
}

/// Whether a message's `content` or a `tool_result`'s `content` holds nothing.
fn empty_content(content: &Value) -> bool {
    match content {
        Value::String(text) => text.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().all(empty_block),
        _ => true,
    }
}

/// The last content block of message `index`.
///
/// Content may be a plain string or an array of blocks; a string is promoted
/// to a one-element array because `cache_control` attaches to a block, and the
/// API rejects it on a bare string. Promoting is a rewrite of the message, not
/// of its meaning.
fn last_block_of(body: &mut Value, index: usize) -> Option<&mut Value> {
    let content = body
        .get_mut("messages")?
        .as_array_mut()?
        .get_mut(index)?
        .get_mut("content")?;
    if content.is_string() {
        let text = content.as_str().unwrap_or_default().to_owned();
        *content = serde_json::json!([{ "type": "text", "text": text }]);
    }
    content.as_array_mut()?.last_mut()
}

/// Whether a message is the synthetic env tail rather than something said.
fn is_env_turn(message: &Value) -> bool {
    message
        .get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(ENV_MARKER))
}

/// Mark the cacheable prefixes on a request body, in place.
///
/// Returns the number of breakpoints written, which is at most
/// [`MAX_BREAKPOINTS`] and may be fewer on a short conversation.
pub fn mark_breakpoints(body: &mut Value) -> usize {
    let mut marked = 0;

    // The last two system blocks — the captured placement. Two rather than one
    // because the first is the identity prefix and the second the instructions:
    // marking both means a change to either cannot discard the cached other.
    let system_len = body.get("system").and_then(Value::as_array).map_or(0, Vec::len);
    for index in (0..system_len).rev().take(2) {
        if body
            .get_mut("system")
            .and_then(Value::as_array_mut)
            .and_then(|a| a.get_mut(index))
            .is_some_and(mark)
        {
            marked += 1;
        }
    }

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        // The last real turn, skipping the env tail this module appends. If
        // the last message is not an env turn, it is the real one.
        let last = messages.len().saturating_sub(1);
        let target = if messages.get(last).is_some_and(is_env_turn) {
            last.checked_sub(1)
        } else {
            Some(last)
        };
        if let Some(index) = target
            && last_block_of(body, index).is_some_and(mark)
        {
            marked += 1;
        }
    }

    // Two system marks and one message mark, which is what the captured
    // requests use. The budget is checked rather than assumed: going over is an
    // API error rather than a truncation, so a future change that adds a fourth
    // position should fail here rather than in production.
    debug_assert!(
        marked <= MAX_BREAKPOINTS,
        "marked {marked} breakpoints, and the provider accepts only {MAX_BREAKPOINTS}"
    );

    marked
}

/// The text of the synthetic state turn.
///
/// Pure, so both backends render it identically and a test can assert on it
/// without building a body. Says who wrote it, because anything reading a
/// transcript — a human, a log grep, or the judge — must not mistake it for
/// something the user typed.
#[must_use]
pub fn env_text(cwd: &str, gate: &str) -> String {
    format!(
        "{ENV_MARKER}cwd=\"{cwd}\" gate=\"{gate}\"/>\n\
         This block is written by the agent harness, not by the user. It records the working \
         directory and the current permission mode."
    )
}

/// Append the synthetic turn carrying live state, and return its text.
///
/// Returned rather than built at the call site so the marker and the rendering
/// cannot drift apart — [`is_env_turn`] recognises what this writes.
///
/// Deliberately a `user` turn: a `system` turn would be another prefix
/// position, and the point of moving it is that it comes after every
/// breakpoint.
pub fn append_env_turn(body: &mut Value, cwd: &str, gate: &str) -> String {
    let text = env_text(cwd, gate);
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(serde_json::json!({ "role": "user", "content": text }));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(messages: &Value) -> Value {
        json!({
            "model": "m",
            "system": [{ "type": "text", "text": "prefix" }],
            "tools": [{ "name": "Read" }, { "name": "Write" }],
            "messages": messages,
        })
    }

    /// Marks per region, as `(system, tools, messages)`.
    ///
    /// Per region rather than a total, because the interesting assertions are
    /// about *where* a mark landed — "one breakpoint" is true of a body that
    /// marked the wrong block, and a total cannot tell the difference.
    fn marks(body: &Value) -> (usize, usize, usize) {
        let region = |value: &Value| {
            value
                .as_array()
                .map_or(0, |a| a.iter().filter(|b| b.get("cache_control").is_some()).count())
        };
        let system = region(&body["system"]);
        let tools = region(&body["tools"]);
        let messages = body["messages"].as_array().map_or(0, |a| {
            a.iter()
                .map(|m| {
                    m["content"]
                        .as_array()
                        .map_or(0, |c| c.iter().filter(|b| b.get("cache_control").is_some()).count())
                })
                .sum()
        });
        (system, tools, messages)
    }

    #[test]
    fn it_marks_the_system_blocks_and_the_last_real_turn() {
        let mut b = body(&json!([
            { "role": "user", "content": "first" },
            { "role": "assistant", "content": "second" },
        ]));
        assert_eq!(mark_breakpoints(&mut b), 2);
        // One system block in the fixture, so one mark there; the tools array
        // is deliberately never marked, matching the captured requests.
        assert_eq!(marks(&b), (1, 0, 1));
        // The last block was promoted from a string so it could carry a mark.
        assert_eq!(b["messages"][1]["content"][0]["text"], "second");
        assert!(b["messages"][1]["content"][0]["cache_control"].is_object());
    }

    /// The captured placement: a real request marked its last *two* system
    /// blocks. Marking only one would discard the identity prefix whenever the
    /// instructions changed, and vice versa.
    #[test]
    fn the_last_two_system_blocks_are_marked() {
        let mut b = json!({
            "system": [
                { "type": "text", "text": "identity" },
                { "type": "text", "text": "instructions" },
                { "type": "text", "text": "env" },
            ],
            "messages": [{ "role": "user", "content": "hi" }],
        });
        mark_breakpoints(&mut b);
        assert!(b["system"][1]["cache_control"].is_object(), "second-to-last");
        assert!(b["system"][2]["cache_control"].is_object(), "last");
        assert!(
            b["system"][0].get("cache_control").is_none(),
            "the oldest block is left alone — only the last two are marked"
        );
    }

    /// The reason [`stabilise_shapes`] exists, as a before-and-after.
    ///
    /// This is the defect it fixes: without it, a message is promoted to a
    /// block array on the turn it is last (because that is the turn that gets
    /// marked) and reverts to a string on every turn after — a shape change
    /// inside the cached prefix, on every single turn, caused by the marking.
    #[test]
    fn a_marked_turn_keeps_its_shape_when_it_stops_being_last() {
        // Turn 1: one message, which is therefore the last real turn.
        let mut first = body(&json!([{ "role": "user", "content": "first" }]));
        stabilise_shapes(&mut first);
        append_env_turn(&mut first, "/w", "open");
        mark_breakpoints(&mut first);
        let shape_of = |b: &Value, i: usize| b["messages"][i]["content"].clone();

        // Turn 2: the same message, now mid-history, plus a new last turn.
        let mut second = body(&json!([
            { "role": "user", "content": "first" },
            { "role": "assistant", "content": "answered" },
            { "role": "user", "content": "second" },
        ]));
        stabilise_shapes(&mut second);
        append_env_turn(&mut second, "/w", "open");
        mark_breakpoints(&mut second);

        // Same shape in both, so the provider sees the same bytes for the same
        // turn. Only the mark differs, and the mark legitimately moves.
        assert_eq!(
            shape_of(&first, 0).as_array().map(Vec::len),
            shape_of(&second, 0).as_array().map(Vec::len),
            "the first turn changed shape between requests:\n{:?}\n{:?}",
            shape_of(&first, 0),
            shape_of(&second, 0)
        );
        assert_eq!(shape_of(&second, 0)[0]["text"], "first");
    }

    /// Stabilising must happen before the state turn is appended, because that
    /// turn is recognised by its bare-string content. A test for the ordering,
    /// since getting it wrong puts the breakpoint on the one message that
    /// changes every request.
    #[test]
    fn the_state_turn_stays_a_string_so_it_can_be_recognised() {
        let mut b = body(&json!([{ "role": "user", "content": "real" }]));
        stabilise_shapes(&mut b);
        append_env_turn(&mut b, "/w", "auto");
        mark_breakpoints(&mut b);

        let messages = b["messages"].as_array().expect("messages");
        let env = messages.last().expect("the state turn");
        assert!(
            env["content"].as_str().is_some_and(|s| s.starts_with(ENV_MARKER)),
            "the state turn must remain a plain string: {}",
            env["content"]
        );
        assert!(
            is_env_turn(env),
            "and must still be recognised as one, or the breakpoint lands on it"
        );
        assert!(
            messages[0]["content"][0]["cache_control"].is_object(),
            "the breakpoint belongs on the last real turn"
        );
    }

    #[test]
    fn stabilising_rewrites_only_bare_strings() {
        let mut b = body(&json!([
            { "role": "user", "content": "plain" },
            { "role": "assistant", "content": [{ "type": "text", "text": "already blocks" }] },
        ]));
        assert_eq!(stabilise_shapes(&mut b), 1, "only the string one is rewritten");
        assert_eq!(b["messages"][0]["content"][0]["text"], "plain");
        assert_eq!(b["messages"][1]["content"][0]["text"], "already blocks");
        // Idempotent: a second pass finds nothing to do.
        assert_eq!(stabilise_shapes(&mut b), 0);
    }

    /// The state turn is rebuilt every request, so a breakpoint on it would
    /// cache the stable history behind a block that changes — every turn paying
    /// for the part that never moves.
    #[test]
    fn the_breakpoint_lands_before_the_env_turn_not_on_it() {
        let mut b = body(&json!([
            { "role": "user", "content": "first" },
            { "role": "assistant", "content": "second" },
        ]));
        append_env_turn(&mut b, "/work", "auto");
        mark_breakpoints(&mut b);

        let messages = b["messages"].as_array().expect("messages");
        let real = &messages[1];
        let env = &messages[2];
        assert!(
            real["content"][0]["cache_control"].is_object(),
            "the last real turn must carry the breakpoint"
        );
        assert!(
            env["content"].as_str().is_some_and(|s| s.starts_with(ENV_MARKER)),
            "the env turn stays a plain string, marking nothing"
        );
        assert_eq!(marks(&b), (1, 0, 1), "messages contribute exactly one breakpoint");
    }

    /// With only the env turn present there is nothing real to anchor to, and
    /// marking it would cache a block that changes every request.
    #[test]
    fn an_env_only_history_marks_nothing_in_messages() {
        let mut b = body(&json!([]));
        append_env_turn(&mut b, "/work", "open");
        mark_breakpoints(&mut b);
        let (system, tools, messages) = marks(&b);
        assert_eq!(messages, 0, "nothing real to anchor to");
        assert_eq!((system, tools), (1, 0), "the system block is still marked");
    }

    /// A classifier-shaped body: no tools, and a system array. It must not
    /// invent a tools breakpoint.
    #[test]
    fn an_empty_tool_list_is_not_marked() {
        let mut b = json!({ "system": [{ "type": "text", "text": "s" }], "tools": [], "messages": [] });
        assert_eq!(mark_breakpoints(&mut b), 1);
        assert_eq!(marks(&b), (1, 0, 0));
    }

    /// Four is the API's limit, and going over is an error rather than a
    /// truncation — so a body that already carries marks must not gain more.
    #[test]
    fn it_never_exceeds_the_provider_limit() {
        let mut b = body(&json!([{ "role": "user", "content": "only" }]));
        let first = mark_breakpoints(&mut b);
        let second = mark_breakpoints(&mut b);
        assert_eq!(first, 2);
        assert_eq!(second, 2, "marking is idempotent, not additive");
        let (system, tools, messages) = marks(&b);
        assert!(
            system + tools + messages <= MAX_BREAKPOINTS,
            "the provider rejects a body carrying more than {MAX_BREAKPOINTS}"
        );
    }

    /// Every marked position must be a real object, or the API rejects the
    /// request rather than ignoring the mark.
    #[test]
    fn every_mark_is_on_an_object() {
        let mut b = body(&json!([{ "role": "user", "content": "x" }]));
        append_env_turn(&mut b, "/w", "plan");
        mark_breakpoints(&mut b);
        for entry in b["system"].as_array().into_iter().flatten() {
            assert!(entry.is_object());
        }
        for entry in b["tools"].as_array().into_iter().flatten() {
            assert!(entry.is_object());
        }
        for message in b["messages"].as_array().into_iter().flatten() {
            if let Some(blocks) = message["content"].as_array() {
                for block in blocks {
                    assert!(block.is_object(), "marks attach to blocks, not strings");
                }
            }
        }
    }

    /// The failure this exists for: a resumed session sent a turn whose only
    /// content was a `thinking` block with no text, and the API answered
    /// `messages.6: all messages must have non-empty content`.
    #[test]
    fn it_drops_a_turn_whose_only_block_is_an_empty_thinking_block() {
        let mut b = body(&json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "", "signature": "sig" }
            ]},
            { "role": "user", "content": [{ "type": "text", "text": "on" }] },
        ]));

        assert_eq!(
            prune_empty_content(&mut b),
            2,
            "the empty block and the turn it emptied"
        );
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2, "the turn that said nothing is gone");
        assert_eq!(messages[1]["content"][0]["text"], "on", "and the rest survives");
    }

    /// An empty block condemns only itself, not the turn it sits in: a turn
    /// that also called a tool is a turn worth keeping.
    #[test]
    fn it_drops_the_empty_block_and_keeps_the_turn() {
        let mut b = body(&json!([
            { "role": "assistant", "content": [
                { "type": "text", "text": "   " },
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": {} },
            ]},
        ]));

        assert_eq!(prune_empty_content(&mut b), 1);
        let blocks = b["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
    }

    /// Removing an empty `tool_result` would orphan its `tool_use`, which the
    /// API rejects in turn — so it is answered instead. This is the same trade
    /// `truncate_at_orphan_tool_use` makes from the other side.
    #[test]
    fn an_empty_tool_result_is_answered_rather_than_orphaned() {
        let mut b = body(&json!([
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": {} }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "" }
            ]},
        ]));

        assert_eq!(prune_empty_content(&mut b), 1);
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2, "both turns survive");
        assert_eq!(messages[1]["content"][0]["content"], NO_OUTPUT);
    }

    /// A body with nothing empty in it must come back byte-identical —
    /// including block types this pass does not model but must not touch.
    #[test]
    fn a_healthy_history_is_left_alone() {
        let mut b = body(&json!([
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "hmm", "signature": "sig" },
                { "type": "text", "text": "hello" },
                { "type": "redacted_thinking", "data": "opaque" },
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "fine" }
            ]},
        ]));
        let before = b.clone();

        assert_eq!(prune_empty_content(&mut b), 0);
        assert_eq!(b, before);
    }

    /// A turn holding nothing but thinking is not a turn.
    ///
    /// This is the shape behind the second 400 — `messages.3`, and `messages.6`
    /// before it — from a resumed Claude Code transcript, where thinking is
    /// written as its own entry. The block is not empty: it is full of the
    /// model's working, so an emptiness check keeps it. The relayed model does
    /// not implement thinking, so the block is dropped in transit and the turn
    /// arrives with no content at all, which is what the API refuses.
    ///
    /// Checked against the transcript that produced it: six such turns, at the
    /// indices the API named, and none left after this.
    #[test]
    fn a_turn_of_nothing_but_thinking_is_dropped() {
        let mut b = body(&json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "let me weigh the options", "signature": "sig" }
            ]},
            { "role": "assistant", "content": [{ "type": "text", "text": "here it is" }] },
        ]));

        assert_eq!(prune_empty_content(&mut b), 2, "the content and the turn it emptied");
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2, "only deliberation went, and the turn with it");
        assert_eq!(messages[1]["content"][0]["text"], "here it is");
        assert!(
            !serde_json::to_string(messages).unwrap().contains("weigh the options"),
            "the deliberation itself must be gone, not merely emptied"
        );
    }

    /// A turn whose thinking sits beside real content keeps both: only a turn that
    /// is *nothing but* deliberation arrives empty, so only that one is dropped.
    /// (That thinking beside content survives untouched is also pinned by
    /// `a_healthy_history_is_left_alone`; this states the reason.)
    #[test]
    fn thinking_beside_real_content_is_kept() {
        let mut b = body(&json!([
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "weighing", "signature": "sig" },
                { "type": "tool_use", "id": "t1", "name": "Read", "input": {} },
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "fine" }
            ]},
        ]));

        assert_eq!(prune_empty_content(&mut b), 0);
        assert_eq!(b["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(b["messages"].as_array().unwrap().len(), 2);
    }

    /// A classifier-shaped body carries no `messages` at all.
    #[test]
    fn a_body_without_messages_is_untouched() {
        let mut b = json!({ "system": [{ "type": "text", "text": "s" }] });
        assert_eq!(prune_empty_content(&mut b), 0);
    }
}
