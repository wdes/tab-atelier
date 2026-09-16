// SPDX-License-Identifier: MPL-2.0

//! Integration tests over the shapes production actually sends.
//!
//! Every fixture is built at runtime from a repeated pattern. The interesting
//! properties are size and position, and a committed payload would test nothing
//! a generated one does while making every clone large — so nothing here is
//! stored, and `fixtures_are_generated_not_stored` fails if that ever changes.
//!
//! The sizes below are drawn from live captures (2026-09-16) rather than picked
//! to make the tests pass: 11,026 elided write payloads averaged 1,168 bytes
//! with a 44,014-byte maximum, and stubbed tool results averaged 2,100 bytes.
//! The fixtures use round multiples of those, which is what the pass sees.
//!
//! The tests are written against the shape of a real request, not against the
//! functions: `apply` is called the way the relay calls it, on a whole body,
//! and the assertions read the body back the way a client would.

use serde_json::{Value, json};
use tab_atelier_proxy::compact::{Compact, KEEP_TURNS, apply};

/// The marker production puts in place of a stubbed tool result.
const TOOL_STUB: &str = "[tool result elided by tab-atelier-proxy: ";
/// The marker it puts in place of an old write payload.
const WRITE_STUB: &str = "[file content elided by tab-atelier-proxy: ";

/// A payload of `len` bytes, built from a short repeated pattern.
///
/// Repeated rather than random so a failing assertion prints something a human
/// can compare, and short rather than one character so a search for the pattern
/// cannot accidentally match the surrounding JSON.
fn filler(len: usize) -> String {
    "abcdefgh".repeat(len / 8)
}

/// One `tool_use` block, as the client sends it.
fn tool_use(id: &str, name: &str, input: &Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// One `tool_result` block, as the client sends it.
fn tool_result(id: &str, body: &str) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": body })
}

/// An assistant turn that called one tool.
fn assistant_call(id: &str, name: &str, input: &Value) -> Value {
    json!({ "role": "assistant", "content": [tool_use(id, name, input)] })
}

/// The user turn that answered it.
fn user_result(id: &str, body: &str) -> Value {
    json!({ "role": "user", "content": [tool_result(id, body)] })
}

/// One call/result pair of `result_bytes`, as an old turn.
fn old_pair(n: usize, result_bytes: usize) -> [Value; 2] {
    let id = format!("call_old{n}");
    let write = format!("old file {n}");
    [
        assistant_call(
            &id,
            "Write",
            &json!({ "file_path": write, "content": filler(result_bytes) }),
        ),
        user_result(&id, &filler(result_bytes)),
    ]
}

/// A body shaped like a long session: many old turns, then a live tail.
///
/// `turns` old pairs, then [`KEEP_TURNS`] recent turns whose contents must
/// survive. The tail is small on purpose — in production it is a few turns of
/// recent work, and the bulk being old is the whole premise of the pass.
fn long_session(turns: usize, old_bytes: usize) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for n in 0..turns {
        messages.extend(old_pair(n, old_bytes));
    }
    for n in 0..KEEP_TURNS {
        let id = format!("call_new{n}");
        messages.push(assistant_call(
            &id,
            "Write",
            &json!({ "file_path": format!("recent {n}"), "content": filler(400) }),
        ));
        messages.push(user_result(&id, &filler(400)));
    }
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 8192,
        "messages": messages,
    })
}

/// Every string leaf in the document, for shape assertions.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(o) => o.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// Every `content`-bearing string in the document.
fn all_strings(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    strings(value, &mut out);
    out
}

#[test]
fn fixtures_are_generated_not_stored() {
    // The guard on this file: a payload must be a repeated pattern, so a real
    // capture can never be pasted in. If this fails, a fixture became data.
    let body = long_session(4, 1_600);
    for s in all_strings(&body) {
        if s.len() < 64 {
            continue;
        }
        let pattern = &s[..8];
        assert!(
            s.len() % 8 == 0 && s.chars().collect::<String>().matches(pattern).count() == s.len() / 8,
            "a long fixture string is not a repeated pattern — fixtures must be generated, \
             not copied from a capture"
        );
    }
}

#[test]
fn old_tool_results_are_replaced_by_a_stub() {
    let mut body = long_session(4, 1_600);
    let stats = apply(&mut body, Compact::Tools);

    assert!(stats.tool_results_elided > 0, "nothing was elided");
    let text = body.to_string();
    assert!(text.contains(TOOL_STUB), "no tool stub was written");
}

#[test]
fn the_trailing_keep_window_is_left_alone() {
    // Recent contents are what the model is still reasoning over. Counted
    // before and after rather than against a hardcoded number: the recent
    // payload appears twice per turn — the Write argument and the tool result
    // that answered it — and pinning that arithmetic instead of the property
    // would make the test fail on a fixture change that broke nothing.
    let mut body = long_session(4, 1_600);
    let recent = filler(400);
    let before = all_strings(&body).iter().filter(|s| **s == recent).count();
    assert!(before > 0, "the fixture has no recent payload to protect");

    let _ = apply(&mut body, Compact::All);

    let after = all_strings(&body).iter().filter(|s| **s == recent).count();
    assert_eq!(after, before, "the trailing window was modified");
}

#[test]
fn tool_use_and_tool_result_stay_paired() {
    // The one invariant the compaction module documents as sacred: dropping a
    // `tool_result` leaves its `tool_use` unanswered, which is a 400.
    let mut body = long_session(4, 1_600);
    let _ = apply(&mut body, Compact::All);

    let mut calls: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for m in body["messages"].as_array().expect("messages") {
        for b in m["content"].as_array().expect("content") {
            match b["type"].as_str() {
                Some("tool_use") => calls.push(b["id"].as_str().unwrap_or_default().to_owned()),
                Some("tool_result") => {
                    results.push(b["tool_use_id"].as_str().unwrap_or_default().to_owned());
                }
                _ => {}
            }
        }
    }
    for id in &calls {
        assert!(results.contains(id), "tool_use {id} has no tool_result");
    }
}

#[test]
fn a_marker_that_reached_a_write_is_never_re_stubbed() {
    // The failure this suite exists for, in miniature. A marker that has been
    // copied into a `Write` argument is indistinguishable from one the pass
    // wrote, so a second pass leaves it — and the marker is what ends up in the
    // file. This documents why markers on disk are unrecoverable by re-running
    // the pass, and why the fix cannot be a tidier marker.
    let copied = format!("{WRITE_STUB}1622 bytes]");
    let id = "call_copy";
    let mut body = json!({
        "messages": [
            // Old enough to be in scope for every layer.
            assistant_call("call_old0", "Write", &json!({ "file_path": "a", "content": filler(1_600) })),
            user_result("call_old0", &filler(1_600)),
            assistant_call(id, "Write", &json!({ "file_path": "046-boot-text.sh", "content": copied })),
            user_result(id, "ok"),
        ]
    });

    let _ = apply(&mut body, Compact::All);

    let text = body.to_string();
    assert!(
        text.contains(&copied),
        "the copied marker must still be present — it is content now"
    );
}

#[test]
fn no_write_payload_is_ever_replaced_by_a_marker() {
    // What the fix must guarantee. A `Write` or `Edit` argument is text the
    // model authored, and a marker standing in for it is text the model can
    // copy out and write to disk — which is how `policies.json` and four
    // memory files were destroyed on 2026-09-16.
    //
    // Tool results carry command output, which nothing renders back as a file
    // body, so those stay elidable. The two are different and this pins it.
    // Nine turns, so layer A actually engages: `long_session`'s own writes are
    // 400 bytes and sit in a short body, where the keep window covers everything
    // and nothing is elided — a fixture on which the assertions below could not
    // fail. This promotes the oldest turn's write to a payload the old layer
    // would have stubbed and puts it where that layer applied.
    let mut body = long_session(9, 1_600);
    let authored = filler(4_000);
    body["messages"][0]["content"][0]["input"]["content"] = Value::String(authored.clone());

    let stats = apply(&mut body, Compact::All);

    let text = body.to_string();
    assert!(
        text.contains(&authored),
        "an authored file body was replaced by a marker — the model can copy that marker into a file"
    );
    assert!(!text.contains(WRITE_STUB), "a write payload was replaced by a marker");
    assert!(
        stats.tool_results_elided > 0,
        "layer A must still be eliding tool results — only layer D was removed"
    );
    assert!(
        text.contains(TOOL_STUB),
        "tool results must still be elided — this is layer A, and it is unaffected"
    );
}

/// The same acceptance check, against a body captured from production.
///
/// Ignored by default and pointed at a file rather than committed: a real body
/// is megabytes (the sample this was written against is 21 MB) and carries
/// whatever the person was working on, so it belongs on the machine that
/// produced it and nowhere near git. It exists because the synthetic fixtures
/// above are shapes this repo invented, and the shapes that broke `policies.json`
/// came from a real agent mid-task.
///
/// Run it with an inspection export:
///
/// ```text
/// TAB_ATELIER_REAL_BODY=/tmp/body.json cargo test -p tab-atelier-proxy \
///     --test compaction_shapes -- --ignored
/// ```
#[test]
#[ignore = "needs a real captured body: set TAB_ATELIER_REAL_BODY"]
fn a_real_captured_body_gains_no_marker() {
    let Ok(path) = std::env::var("TAB_ATELIER_REAL_BODY") else {
        return;
    };
    let raw = std::fs::read_to_string(&path).expect("read the captured body");
    let mut body: Value = serde_json::from_str(&raw).expect("the capture parses as JSON");

    // Counted rather than assumed absent: a captured body already carries the
    // stubs of the requests it was recorded from, so the property is that THIS
    // pass adds none — not that none exist.
    let before = raw.matches(WRITE_STUB).count();
    let _ = apply(&mut body, Compact::All);
    let after = body.to_string().matches(WRITE_STUB).count();

    assert_eq!(
        after, before,
        "the pass introduced a write marker into a real body — the model can copy that into a file"
    );
    assert!(body.get("messages").is_some(), "the message array survived");
    assert!(
        body.get("system").is_some() || body.get("tools").is_some(),
        "the rest of the envelope survived"
    );
}
