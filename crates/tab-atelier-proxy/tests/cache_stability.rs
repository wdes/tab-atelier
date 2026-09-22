// SPDX-License-Identifier: MPL-2.0

//! Guard the cache: shaping a new turn must not rewrite what was already sent.
//!
//! Every request the relay forwards is shaped first — the model is renamed,
//! history is compacted, the tool list is governed, the Claude Code identity is
//! rewritten for a vendor that is not Anthropic. Those are all edits, and a
//! prompt is only cheap to re-send while it still *starts* with what went last
//! time: a provider whose cache matches a strict prefix re-processes everything
//! from the first change to the end of the prompt.
//!
//! The unit tests beside each pass cover what it removes. They cannot cover
//! this, because it is a property of the *pipeline*: a pass can be correct in
//! isolation and still rewrite history in place, which its own tests will not
//! notice. The measured cost of getting it wrong, on the fleet's non-Anthropic
//! traffic, is ~6,400 tokens re-billed every turn at 391,000 of context, flat
//! across 25,000 to 744,000 — about the six turns the window keeps, paid again
//! and again.
//!
//! So these tests drive [`shape_body`] itself rather than a copy of it, and hold
//! one property: turn N+1's output begins with all of turn N's output.

use bytes::Bytes;
use serde_json::{Value, json};
use tab_atelier_proxy::classifier;
use tab_atelier_proxy::compact::{self, Compact};
use tab_atelier_proxy::http::controllers::relay::shape_body;
use tab_atelier_proxy::identity::Vendor;
use tab_atelier_proxy::provider::Class;
use tab_atelier_proxy::routing;
use tab_atelier_proxy::tools;

/// Turns to push. Comfortably past `compact::KEEP_TURNS`, because the defect
/// this guards against only appears once the window starts moving.
const TURNS: usize = 24;

fn route() -> routing::Route {
    routing::Route {
        provider_id: "deepseek".to_owned(),
        model_id: "deepseek-flash".to_owned(),
        class: Class::Balanced,
        // Work, not the classifier: the classifier is exempt from compaction by
        // construction, so it would shape nothing and every case would pass
        // without exercising anything.
        kind: classifier::Kind::Work,
        changed_from: None,
        reason: None,
    }
}

fn shape(body: &Value, vendor: Vendor, level: Compact, policy: &tools::Policy) -> Value {
    let (out, _, _) = shape_body(
        &Bytes::from(serde_json::to_vec(body).expect("body serialises")),
        &route(),
        "deepseek-flash",
        level,
        policy,
        true,
        vendor,
    );
    serde_json::from_slice(&out).expect("shaped body is JSON")
}

/// A conversation shaped like the client sends: an identity system block, a tool
/// list worth governing, and one turn.
fn conversation() -> Value {
    json!({
        "model": "deepseek-flash",
        "max_tokens": 1024,
        "system": [{
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude."
        }],
        "tools": [
            {"name": "Read", "description": "Read a file.", "input_schema": {"type": "object"}},
            {"name": "Write", "description": "Write a file.", "input_schema": {"type": "object"}},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "start"}]}],
    })
}

/// One more turn: a tool result big enough for the size rule to care, then the
/// assistant turn answering it — with a thinking block, so both compaction
/// layers have something to act on.
fn push_turn(body: &mut Value, turn: usize) {
    let list = body["messages"].as_array_mut().expect("messages is an array");
    list.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": format!("call_{turn:02}"),
            "content": "r".repeat(2_000),
        }],
    }));
    list.push(json!({
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "t".repeat(500), "signature": format!("sig{turn:02}")},
            {"type": "tool_use", "id": format!("call_{turn:02}"), "name": "Read",
             "input": {"file_path": "/tmp/x"}},
        ],
    }));
}

fn messages(body: &Value) -> Vec<Value> {
    body["messages"].as_array().cloned().unwrap_or_default()
}

fn wire(message: &Value) -> String {
    serde_json::to_string(message).expect("a message serialises")
}

/// Every combination of the switches that can rewrite a body, because they fail
/// differently and a fix for one need not cover another: compaction moves a
/// window through history, the tool pass edits the list in front of it, and the
/// identity pass edits the system prompt in front of that.
fn cases() -> Vec<(&'static str, Vendor, Compact, tools::Policy)> {
    let mut added = tools::Policy::default();
    added.add.push(json!({
        "name": "LocalThing",
        "description": "A tool the relay adds.",
        "input_schema": {"type": "object"},
    }));
    let mut disabled = tools::Policy::default();
    disabled.disable.push("Write".to_owned());

    vec![
        (
            "anthropic, compaction off",
            Vendor::Anthropic,
            Compact::None,
            tools::Policy::default(),
        ),
        (
            "deepseek, compaction off",
            Vendor::Deepseek,
            Compact::None,
            tools::Policy::default(),
        ),
        (
            "deepseek, compaction: tools",
            Vendor::Deepseek,
            Compact::Tools,
            tools::Policy::default(),
        ),
        (
            "deepseek, compaction: tools+thinking",
            Vendor::Deepseek,
            Compact::ToolsThinking,
            tools::Policy::default(),
        ),
        (
            "deepseek, compaction: all",
            Vendor::Deepseek,
            Compact::All,
            tools::Policy::default(),
        ),
        (
            "anthropic, compaction: all",
            Vendor::Anthropic,
            Compact::All,
            tools::Policy::default(),
        ),
        (
            "openai, compaction: all",
            Vendor::Openai,
            Compact::All,
            tools::Policy::default(),
        ),
        ("deepseek, tool added", Vendor::Deepseek, Compact::Tools, added),
        ("deepseek, tool disabled", Vendor::Deepseek, Compact::Tools, disabled),
    ]
}

/// The property: wherever shaping does rewrite a message the client already
/// sent, it does so a batch at a time — not once per turn.
///
/// This is deliberately not "never rewrites an already-sent message". A message
/// is sent the moment it appears, so compacting it later is necessarily a
/// rewrite of it, and demanding none would be demanding that compaction not
/// happen. What can be held is how *often* the prefix moves: once per batch of
/// `compact::ELIDE_BATCH` turns rather than once per turn, which divides the
/// re-processing a strict prefix cache charges for by that factor.
///
/// The first version of this test asserted the impossible property, and passed
/// only because its turn count never crossed a boundary — which is exactly the
/// kind of guard that reads as protection and provides none.
#[test]
fn the_shaping_boundary_advances_a_batch_at_a_time() {
    let mut rewrote_anywhere = false;

    for (label, vendor, level, policy) in cases() {
        let mut raw = conversation();
        let mut sent: Vec<String> = Vec::new();
        let mut rewrote_on: Vec<usize> = Vec::new();

        for turn in 0..TURNS {
            let shaped = shape(&raw, vendor, level, &policy);
            let now: Vec<String> = messages(&shaped).iter().map(wire).collect();

            if sent
                .iter()
                .enumerate()
                .any(|(i, was)| now.get(i).is_some_and(|message| message != was))
            {
                rewrote_on.push(turn);
            }
            sent = now;
            push_turn(&mut raw, turn);
        }

        if rewrote_on.is_empty() {
            // A level that compacts nothing has no boundary to move. That it
            // removes nothing is asserted separately, below.
            continue;
        }
        rewrote_anywhere = true;

        for pair in rewrote_on.windows(2) {
            let apart = pair[1] - pair[0];
            assert!(
                apart >= compact::ELIDE_BATCH - 1,
                "{label}: the shaping boundary advanced twice within {apart} turn(s) — on turns \
                 {rewrote_on:?}. It is meant to advance a batch of {} at a time. Advancing it per \
                 turn rewrites already-sent content on every turn, and a strict prefix cache \
                 re-processes everything behind each rewrite, so the retained window is billed \
                 again and again.",
                compact::ELIDE_BATCH
            );
        }
    }

    assert!(
        rewrote_anywhere,
        "no case rewrote anything over {TURNS} turns, so this test asserted nothing"
    );
}

/// The parts in FRONT of the conversation must not move as it grows either.
///
/// `tools[]` and `system[]` precede every message, so a change there invalidates
/// the entire prompt — the most expensive place to be volatile and the easiest
/// to be by accident, since either can be built from live state.
#[test]
fn the_prefix_ahead_of_the_messages_does_not_move_as_the_conversation_grows() {
    for (label, vendor, level, policy) in cases() {
        let mut raw = conversation();
        let mut first: Option<(Value, Value)> = None;
        for turn in 0..TURNS {
            let shaped = shape(&raw, vendor, level, &policy);
            let front = (shaped["tools"].clone(), shaped["system"].clone());
            match &first {
                None => first = Some(front),
                Some(expected) => assert_eq!(
                    &front, expected,
                    "{label}: turn {turn} changed the tool list or the system prompt. Anything \
                     ahead of the conversation invalidates every message behind it."
                ),
            }
            push_turn(&mut raw, turn);
        }
    }
}

/// The levels must actually remove something, or the tests above hold vacuously.
#[test]
fn the_compaction_levels_remove_something() {
    let mut raw = conversation();
    for turn in 0..TURNS {
        push_turn(&mut raw, turn);
    }
    let untouched = messages(&raw);

    for (label, level) in [
        ("tools", Compact::Tools),
        ("tools+thinking", Compact::ToolsThinking),
        ("all", Compact::All),
    ] {
        let shaped = shape(&raw, Vendor::Deepseek, level, &tools::Policy::default());
        assert_ne!(
            messages(&shaped).len(),
            0,
            "{label}: the shaped body lost its messages entirely"
        );
        assert_ne!(
            messages(&shaped),
            untouched,
            "compaction level {label} left a {TURNS}-turn conversation untouched, so the \
             append-only tests would pass for that level without exercising it"
        );
    }

    // And off is off: a level that is `None` must not be quietly compacting.
    let shaped = shape(&raw, Vendor::Deepseek, Compact::None, &tools::Policy::default());
    assert_eq!(
        messages(&shaped),
        untouched,
        "compaction is off but the conversation was rewritten anyway"
    );
}
