// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! 🐊 aligator — a deterministic input router (PoC for #35).
//!
//! Sibling of the ⛑ `brain` rescue tab. Where `brain` *reactively*
//! pattern-matches agent failures and injects `continue\r`, `aligator`
//! drains a typed **swamp** queue and types each entry's `input` into the
//! target tab via `POST /tabs/by-id/<uuid>/input`. A queue-driven,
//! cursor-based input router so any producer (a script, a peer agent, a cron)
//! can leave a message for tab X and have it delivered on the next round.
//!
//! Designed to run AS a tab, exactly like `brain`: `tab-atelier aligator`,
//! its log becomes the tab's scrollback (OSC-2 titled "🐊 aligator").
//!
//! **Swamp = a dedicated typed file** (`<state>/tab-atelier/swamp.jsonl`,
//! decided in #35 option B) — one JSON object per line, appended by the
//! `tab-atelier swamp <tab> "<text>"` producer. A dedicated typed file keeps
//! the routing key a real field (never a fragile `"<uuid> -> <text>"` split)
//! and decouples machine routing from the human-facing `note` blackboard.
//!
//! **Confused-deputy guard:** unlike `brain` (bounded to `continue\r`),
//! aligator types *arbitrary* text, so it only ever delivers to a **live
//! Claude agent tab** (`agent_kind == "claude"` + a non-empty
//! `agent_session_id`) — the same gate `brain` uses. It refuses to type into a
//! plain shell / human terminal. See #35 for the wider safety discussion.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::share_link::{Endpoint, agent, discover_endpoint};

const DEFAULT_INTERVAL_SECS: u64 = 5;
/// Delay before the submitting Enter, so the typed text is ingested as one
/// paste before `\r` lands (see the dispatch paste-submit fix, #31/#32). A
/// fixed floor for the PoC; a follow-up should reuse dispatch's settle poll.
const SUBMIT_DELAY: Duration = Duration::from_millis(400);

/// One swamp entry — a request to type `input` into tab `tab`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwampEntry {
    /// Unix seconds when enqueued.
    pub ts: u64,
    /// Target tab UUID (routing key — a real field, not a parsed string).
    pub tab: String,
    /// Text to type into the tab's input.
    pub input: String,
    /// Whether to press Enter after the text (default true).
    #[serde(default = "default_true")]
    pub submit: bool,
    /// Who enqueued it, if given (audit / `--from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

const fn default_true() -> bool {
    true
}

fn state_file(name: &str) -> PathBuf {
    crate::platform::state_base_dir().join("tab-atelier").join(name)
}

fn swamp_path() -> PathBuf {
    state_file("swamp.jsonl")
}

fn cursor_path() -> PathBuf {
    state_file("aligator.cursor")
}

/// Parse a swamp body into entries, skipping blank / unparseable lines (a
/// half-written line from a racing appender is dropped, not fatal — same
/// tolerance as the `note` blackboard).
#[must_use]
pub fn parse_swamp(body: &str) -> Vec<SwampEntry> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<SwampEntry>(l).ok())
        .collect()
}

/// One swamp entry as a JSONL line (trailing newline included).
#[must_use]
pub fn encode_swamp_line(e: &SwampEntry) -> String {
    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// The subset of a `/tabs` row the guard needs.
#[derive(Debug, Deserialize)]
struct TabInfo {
    id: String,
    #[serde(default)]
    agent_kind: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabsResponse {
    tabs: Vec<TabInfo>,
}

/// The confused-deputy guard: `uuid` must be a **live Claude agent tab**
/// (`agent_kind == "claude"` AND a non-empty session id). Arbitrary text is
/// only ever delivered to a tab that expects programmatic input — never a
/// plain shell or a human terminal. Mirrors `brain`'s own target gate.
#[must_use]
fn is_deliverable(tabs: &[TabInfo], uuid: &str) -> bool {
    tabs.iter().any(|t| {
        t.id == uuid
            && t.agent_kind.as_deref() == Some("claude")
            && !t.agent_session_id.as_deref().unwrap_or("").is_empty()
    })
}

/// Clamp a persisted cursor to `[0, len]` — if the swamp was truncated or
/// rotated under us, a stale cursor past the end must not skip fresh entries
/// (reset to the new end) nor panic on the slice.
#[must_use]
pub fn clamp_cursor(cursor: usize, len: usize) -> usize {
    cursor.min(len)
}

fn read_cursor() -> usize {
    std::fs::read_to_string(cursor_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_cursor(n: usize) {
    let path = cursor_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, n.to_string());
}

fn send_input(ep: &Endpoint, uuid: &str, bytes: &[u8]) -> Result<(), String> {
    agent()
        .post(format!("{}/tabs/by-id/{uuid}/input", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/octet-stream")
        .send(bytes)
        .map_err(|e| format!("POST input for {uuid}: {e}"))?;
    Ok(())
}

/// One round: read new swamp entries past the cursor, deliver each to a
/// deliverable tab (guarded), advance the cursor after each (exactly-once
/// best effort — a crash mid-round re-reads from the persisted cursor).
fn tick(cursor: &mut usize) -> Result<(), String> {
    let body = std::fs::read_to_string(swamp_path()).unwrap_or_default();
    let entries = parse_swamp(&body);
    *cursor = clamp_cursor(*cursor, entries.len());
    if *cursor >= entries.len() {
        return Ok(()); // nothing new this round
    }

    // Only hit the daemon when there's actually work.
    let ep = discover_endpoint()?;
    let ag = agent();
    let auth = format!("Bearer {}", ep.token);
    let tabs: TabsResponse = ag
        .get(format!("{}/tabs", ep.url))
        .header("Authorization", &auth)
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("parse /tabs: {e}"))?;

    for entry in &entries[*cursor..] {
        if is_deliverable(&tabs.tabs, &entry.tab) {
            match send_input(&ep, &entry.tab, entry.input.as_bytes()) {
                Ok(()) => {
                    if entry.submit {
                        std::thread::sleep(SUBMIT_DELAY);
                        let _ = send_input(&ep, &entry.tab, b"\r");
                    }
                    println!(
                        "🐊 aligator: {name:<36} ← {n} byte(s){submit}",
                        name = entry.tab,
                        n = entry.input.len(),
                        submit = if entry.submit { " + ⏎" } else { "" },
                    );
                }
                Err(e) => eprintln!("🐊 aligator: deliver failed for {}: {e}", entry.tab),
            }
        } else {
            // Guard tripped: not a live Claude tab. Log + skip (still advance
            // the cursor so a bad target doesn't wedge the queue).
            println!(
                "🐊 aligator: SKIP {} — not a live Claude tab (confused-deputy guard)",
                entry.tab
            );
        }
        *cursor += 1;
        write_cursor(*cursor);
    }
    Ok(())
}

/// Append `brain-crash.log`-style trace when a tick panics.
fn crash_log(msg: &str) {
    let path = state_file("aligator-crash.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut once = false;
    let mut interval = DEFAULT_INTERVAL_SECS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => once = true,
            "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => interval = n,
                    _ => {
                        eprintln!("aligator: --interval expects a number >= 1");
                        return 2;
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier aligator [--once] [--interval SECS]\n\
                     Drains the swamp queue ({swamp}) and types each entry's input into\n\
                     the target tab. Delivers ONLY to a live Claude agent tab\n\
                     (agent_kind == \"claude\" + a session) — never a plain shell.\n\
                     Cursor-based (exactly-once best effort), one round every {interval}s.\n\
                     Enqueue with: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]",
                    swamp = swamp_path().display(),
                );
                return 0;
            }
            other => {
                eprintln!("aligator: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    print!("\x1b]2;\u{1f40a} aligator\x07");
    println!(
        "\u{1f40a} aligator — draining {swamp} every {interval}s (Claude-tab guard on)",
        swamp = swamp_path().display(),
    );

    let mut cursor = read_cursor();
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(&mut cursor)));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("🐊 aligator: round failed: {e}"),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(non-string panic payload)");
                crash_log(&format!("tick panicked (recovered): {msg}"));
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("🐊 aligator: tick PANICKED, recovered: {msg}\n").as_bytes(),
                );
            }
        }
        if once {
            return 0;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// `tab-atelier swamp <tab-uuid> "<text>" [--no-submit] [--from NAME]` — the
/// producer: append one entry to the swamp for aligator to deliver.
#[must_use]
pub fn run_swamp(args: &[String]) -> i32 {
    let mut tab: Option<&str> = None;
    let mut input: Option<&str> = None;
    let mut submit = true;
    let mut from: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-submit" => submit = false,
            "--from" => from = it.next().cloned(),
            "-h" | "--help" => {
                eprintln!("usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit] [--from NAME]");
                return 0;
            }
            other if tab.is_none() => tab = Some(other),
            other if input.is_none() => input = Some(other),
            other => {
                eprintln!("swamp: unexpected argument: {other}");
                return 2;
            }
        }
    }
    let (Some(tab), Some(input)) = (tab, input) else {
        eprintln!("swamp: usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]");
        return 2;
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let entry = SwampEntry {
        ts,
        tab: tab.to_string(),
        input: input.to_string(),
        submit,
        from,
    };
    let path = swamp_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(encode_swamp_line(&entry).as_bytes()) {
                eprintln!("swamp: write {}: {e}", path.display());
                return 1;
            }
            println!("🐊 swamped → {tab} ({} byte(s){})", input.len(), if submit { " + ⏎" } else { "" });
            0
        }
        Err(e) => {
            eprintln!("swamp: open {}: {e}", path.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str, kind: Option<&str>, session: Option<&str>) -> TabInfo {
        TabInfo {
            id: id.into(),
            agent_kind: kind.map(Into::into),
            agent_session_id: session.map(Into::into),
        }
    }

    #[test]
    fn guard_delivers_only_to_live_claude_tabs() {
        let tabs = vec![
            tab("claude-live", Some("claude"), Some("sess-1")),
            tab("claude-nosession", Some("claude"), None),
            tab("shell", None, None),
            tab("catbus", Some("catbus"), Some("s2")),
        ];
        assert!(is_deliverable(&tabs, "claude-live"));
        // agent_kind claude but no live session → refuse.
        assert!(!is_deliverable(&tabs, "claude-nosession"));
        // plain shell / human terminal → refuse (the whole point).
        assert!(!is_deliverable(&tabs, "shell"));
        // a different agent kind → refuse (guard is claude-only for v1).
        assert!(!is_deliverable(&tabs, "catbus"));
        // unknown uuid → refuse.
        assert!(!is_deliverable(&tabs, "ghost"));
    }

    #[test]
    fn parse_swamp_skips_blank_and_garbage_lines() {
        let body = "\
{\"ts\":1,\"tab\":\"a\",\"input\":\"hi\",\"submit\":true}

not json at all
{\"ts\":2,\"tab\":\"b\",\"input\":\"yo\"}
";
        let e = parse_swamp(body);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].tab, "a");
        assert_eq!(e[0].input, "hi");
        // `submit` defaults to true when omitted.
        assert!(e[1].submit);
        assert_eq!(e[1].tab, "b");
    }

    #[test]
    fn encode_roundtrips_through_parse() {
        let e = SwampEntry {
            ts: 42,
            tab: "uuid-x".into(),
            input: "run the tests".into(),
            submit: false,
            from: Some("bot-orc".into()),
        };
        let line = encode_swamp_line(&e);
        assert!(line.ends_with('\n'));
        let back = parse_swamp(&line);
        assert_eq!(back, vec![e]);
    }

    #[test]
    fn cursor_clamps_when_swamp_shrinks() {
        // Persisted cursor past the end (swamp truncated/rotated) → clamp to
        // the new length so we neither panic nor skip.
        assert_eq!(clamp_cursor(9, 3), 3);
        assert_eq!(clamp_cursor(2, 3), 2);
        assert_eq!(clamp_cursor(0, 0), 0);
    }
}
