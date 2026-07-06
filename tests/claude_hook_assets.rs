// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Integration test crate — `.unwrap()` is idiomatic here (the crate-wide deny
// in Cargo.toml also covers `tests/`, which never sets `cfg(test)`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Shape tests for `assets/claude-code-managed-settings.json` — the
//! system-wide Claude Code hooks file we ship at
//! `/etc/claude-code/managed-settings.json`.
//!
//! Regression guard for the shipped-with-wrong-matcher bug
//! (`bd23815`): the file used to carry `"matcher": ""`, which matches
//! NOTHING in Claude Code's hook spec, so none of the six hooks ever
//! fired. These tests pin the schema we expect Claude Code to read.

use serde_json::Value;

const MANAGED: &str = include_str!("../assets/claude-code-managed-settings.json");

fn parsed() -> Value {
    serde_json::from_str(MANAGED).expect("managed-settings.json must be valid JSON")
}

#[test]
fn file_parses_as_object_with_hooks_key() {
    let v = parsed();
    let obj = v.as_object().expect("top-level must be an object");
    assert!(obj.contains_key("hooks"), "missing `hooks` key");
}

#[test]
fn every_advertised_event_is_present() {
    let v = parsed();
    let hooks = v
        .get("hooks")
        .and_then(Value::as_object)
        .expect("hooks must be an object");
    for event in [
        "SessionStart",
        "PreToolUse",
        "PostToolUse",
        "Notification",
        "Stop",
        "SessionEnd",
    ] {
        let arr = hooks
            .get(event)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing event {event}"));
        assert!(!arr.is_empty(), "event {event} has no entries");
    }
}

#[test]
fn every_matcher_is_star_not_empty_string() {
    // The shipped file used to have `"matcher": ""`. That matches
    // nothing, so the hooks never ran. Pin `*` (or omitted) here
    // forever.
    let v = parsed();
    let hooks = v.get("hooks").and_then(Value::as_object).unwrap();
    for (event, arr) in hooks {
        for (idx, entry) in arr.as_array().unwrap().iter().enumerate() {
            let Some(matcher) = entry.get("matcher") else {
                continue; // omitted is also "match all" per the spec
            };
            let s = matcher.as_str().unwrap_or("");
            assert_eq!(
                s, "*",
                "event {event} entry {idx}: matcher must be `*` or omitted, got {s:?} — empty string matches nothing"
            );
        }
    }
}

#[test]
fn every_entry_runs_the_bridge_subcommand() {
    let v = parsed();
    let hooks = v.get("hooks").and_then(Value::as_object).unwrap();
    for (event, arr) in hooks {
        for (idx, entry) in arr.as_array().unwrap().iter().enumerate() {
            let inner = entry.get("hooks").and_then(Value::as_array).unwrap_or_else(|| {
                panic!("event {event} entry {idx} missing inner `hooks` array");
            });
            assert!(!inner.is_empty(), "event {event} entry {idx}: empty hooks array");
            for (j, h) in inner.iter().enumerate() {
                assert_eq!(
                    h.get("type").and_then(Value::as_str),
                    Some("command"),
                    "event {event} entry {idx}/{j}: type must be `command`"
                );
                let cmd = h.get("command").and_then(Value::as_str).unwrap_or("");
                assert!(
                    cmd.starts_with("tab-atelier-headless claude-hook "),
                    "event {event} entry {idx}/{j}: command must dispatch through claude-hook, got {cmd:?}"
                );
            }
        }
    }
}
