// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! 🐊 aligator — a deterministic input router (proof-of-concept for #35).
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
//!
//! The I/O-free decision logic (arg parsing, per-round planning, line
//! encoding) is factored into pure functions so it's unit-tested without a
//! live daemon; `run`/`tick`/`run_swamp` are thin wrappers that add the HTTP
//! and filesystem effects.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::share_link::{Endpoint, agent, discover_endpoint};

const DEFAULT_INTERVAL_SECS: u64 = 5;
/// Delay before the submitting Enter, so the typed text is ingested as one
/// paste before `\r` lands (see the dispatch paste-submit fix, #31/#32). A
/// fixed floor here; a follow-up should reuse dispatch's settle poll.
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

/// The entries a compaction KEEPS: the undelivered tail (`entries[cursor..]`).
///
/// The swamp file is rewritten to just these and the cursor reset to 0, so the
/// file stays bounded AND a future cursor reset can't re-deliver already-
/// delivered entries — there are none left to re-deliver. Pure, so the "no
/// re-delivery / no loss / coherent cursor" invariants are unit-testable.
#[must_use]
pub fn compact(entries: &[SwampEntry], cursor: usize) -> Vec<SwampEntry> {
    let start = clamp_cursor(cursor, entries.len());
    entries[start..].to_vec()
}

/// Options parsed from `aligator [--once] [--interval SECS]`.
#[derive(Debug, PartialEq, Eq)]
pub struct RunOpts {
    pub once: bool,
    pub interval: u64,
}

/// Pure arg parser for `run`, so its branch logic is testable without looping.
/// Returns the options, or an exit code (`0` for `--help`, `2` for a bad arg)
/// with the message already printed.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on an unknown argument
/// or a non-numeric / zero `--interval`.
pub fn parse_run_opts(args: &[String]) -> Result<RunOpts, i32> {
    let mut opts = RunOpts {
        once: false,
        interval: DEFAULT_INTERVAL_SECS,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => opts.once = true,
            "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => opts.interval = n,
                    _ => {
                        eprintln!("aligator: --interval expects a number >= 1");
                        return Err(2);
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier aligator [--once] [--interval SECS]\n\
                     Drains the swamp queue and types each entry's input into the target\n\
                     tab. Delivers ONLY to a live Claude agent tab (agent_kind == \"claude\"\n\
                     + a session) — never a plain shell. Cursor-based (exactly-once best\n\
                     effort), one round every {DEFAULT_INTERVAL_SECS}s by default.\n\
                     Enqueue with: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]"
                );
                return Err(0);
            }
            other => {
                eprintln!("aligator: unknown argument: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    Ok(opts)
}

/// What to do with one swamp entry this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Deliver `input` (+ Enter if `submit`) to a live Claude tab.
    Deliver {
        index: usize,
        tab: String,
        input: String,
        submit: bool,
    },
    /// Target isn't a live Claude tab — log + skip (guard tripped).
    Skip { index: usize, tab: String },
}

/// Plan one round: decide deliver-vs-skip for every entry past `cursor`.
///
/// Pure — the HTTP calls (fetch `/tabs`, POST input) live in [`tick`]; this is
/// the routing logic, unit-tested with a fake predicate. `index` is the entry's
/// absolute position (the new cursor is the last index + 1).
#[must_use]
pub fn plan_round(entries: &[SwampEntry], cursor: usize, is_ok: impl Fn(&str) -> bool) -> Vec<Decision> {
    let start = clamp_cursor(cursor, entries.len());
    entries[start..]
        .iter()
        .enumerate()
        .map(|(offset, e)| {
            let index = start + offset;
            if is_ok(&e.tab) {
                Decision::Deliver {
                    index,
                    tab: e.tab.clone(),
                    input: e.input.clone(),
                    submit: e.submit,
                }
            } else {
                Decision::Skip {
                    index,
                    tab: e.tab.clone(),
                }
            }
        })
        .collect()
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

/// Compact the swamp file after a fully-drained round: re-read it (to keep any
/// entry a producer appended DURING the round), rewrite it atomically to just
/// the undelivered tail past `*cursor`, then reset the cursor to 0. Bounds the
/// file and removes the "cursor reset → re-deliver stale" hazard. `*cursor` is
/// left at 0 to match the shortened file.
///
/// `ponytail:` a producer append landing in the tiny window between the re-read
/// and the rename is lost — inherent to any read-modify-write on the append log
/// without a lock; the window is a few syscalls wide. Upgrade = an flock.
fn compact_swamp(cursor: &mut usize) {
    let path = swamp_path();
    let fresh = parse_swamp(&std::fs::read_to_string(&path).unwrap_or_default());
    let kept = compact(&fresh, *cursor);
    let body: String = kept.iter().map(encode_swamp_line).collect();
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        *cursor = 0;
        write_cursor(0);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
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

/// One round: read new swamp entries past the cursor, plan deliver/skip
/// (guarded), execute deliveries over HTTP, advance the cursor after each
/// (exactly-once best effort — a crash mid-round re-reads from the persisted
/// cursor).
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

    for decision in plan_round(&entries, *cursor, |uuid| is_deliverable(&tabs.tabs, uuid)) {
        match decision {
            Decision::Deliver {
                index,
                tab,
                input,
                submit,
            } => match send_input(&ep, &tab, input.as_bytes()) {
                Ok(()) => {
                    if submit {
                        std::thread::sleep(SUBMIT_DELAY);
                        let _ = send_input(&ep, &tab, b"\r");
                    }
                    println!(
                        "🐊 aligator: {tab:<36} ← {n} byte(s){s}",
                        n = input.len(),
                        s = if submit { " + ⏎" } else { "" },
                    );
                    *cursor = index + 1;
                    write_cursor(*cursor);
                }
                Err(e) => {
                    eprintln!("🐊 aligator: deliver failed for {tab}: {e}");
                    // Leave the cursor before this entry: retry next round.
                    return Ok(());
                }
            },
            Decision::Skip { index, tab } => {
                println!("🐊 aligator: SKIP {tab} — not a live Claude tab (confused-deputy guard)");
                *cursor = index + 1;
                write_cursor(*cursor);
            }
        }
    }
    // Round fully drained (a deliver failure returns early, before this): compact
    // the swamp to its undelivered tail so it stays bounded and a future cursor
    // reset can't re-deliver already-delivered entries.
    compact_swamp(cursor);
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
    let opts = match parse_run_opts(args) {
        Ok(o) => o,
        Err(code) => return code,
    };

    print!("\x1b]2;\u{1f40a} aligator\x07");
    println!(
        "\u{1f40a} aligator — draining {swamp} every {interval}s (Claude-tab guard on)",
        swamp = swamp_path().display(),
        interval = opts.interval,
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
        if opts.once {
            return 0;
        }
        std::thread::sleep(Duration::from_secs(opts.interval));
    }
}

/// A parsed `swamp` producer request (minus the timestamp, stamped at write).
#[derive(Debug, PartialEq, Eq)]
pub struct SwampArgs {
    pub tab: String,
    pub input: String,
    pub submit: bool,
    pub from: Option<String>,
}

/// Pure arg parser for `swamp <tab> "<text>" [--no-submit] [--from NAME]`.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on a missing
/// tab/text or an unexpected extra argument.
pub fn parse_swamp_args(args: &[String]) -> Result<SwampArgs, i32> {
    let mut tab: Option<String> = None;
    let mut input: Option<String> = None;
    let mut submit = true;
    let mut from: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-submit" => submit = false,
            "--from" => from = it.next().cloned(),
            "-h" | "--help" => {
                eprintln!("usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit] [--from NAME]");
                return Err(0);
            }
            other if tab.is_none() => tab = Some(other.to_string()),
            other if input.is_none() => input = Some(other.to_string()),
            other => {
                eprintln!("swamp: unexpected argument: {other}");
                return Err(2);
            }
        }
    }
    if let (Some(tab), Some(input)) = (tab, input) {
        Ok(SwampArgs {
            tab,
            input,
            submit,
            from,
        })
    } else {
        eprintln!("swamp: usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]");
        Err(2)
    }
}

/// Append one entry to a swamp file (create + append, line-atomic like the
/// `note` blackboard). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / write I/O error.
pub fn append_swamp_line(path: &Path, entry: &SwampEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_swamp_line(entry).as_bytes())
}

/// `tab-atelier swamp <tab-uuid> "<text>" [--no-submit] [--from NAME]` — the
/// producer: append one entry to the swamp for aligator to deliver.
#[must_use]
pub fn run_swamp(args: &[String]) -> i32 {
    let parsed = match parse_swamp_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let entry = SwampEntry {
        ts,
        tab: parsed.tab.clone(),
        input: parsed.input.clone(),
        submit: parsed.submit,
        from: parsed.from,
    };
    match append_swamp_line(&swamp_path(), &entry) {
        Ok(()) => {
            println!(
                "🐊 swamped → {tab} ({n} byte(s){s})",
                tab = parsed.tab,
                n = parsed.input.len(),
                s = if parsed.submit { " + ⏎" } else { "" },
            );
            0
        }
        Err(e) => {
            eprintln!("swamp: write {}: {e}", swamp_path().display());
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

    fn entry(tab: &str, input: &str, submit: bool) -> SwampEntry {
        SwampEntry {
            ts: 0,
            tab: tab.into(),
            input: input.into(),
            submit,
            from: None,
        }
    }

    #[test]
    fn compact_keeps_undelivered_drops_delivered_and_is_cursor_coherent() {
        let entries = vec![
            entry("t0", "a", true), // 0 delivered
            entry("t1", "b", true), // 1 delivered
            entry("t2", "c", true), // 2 UNdelivered
            entry("t3", "d", true), // 3 UNdelivered
        ];
        let cursor = 2; // delivered [0,1], undelivered [2,3]
        let kept = compact(&entries, cursor);
        // (b) no loss of undelivered — exactly the tail survives, in order.
        assert_eq!(kept, entries[2..].to_vec());
        // (a) no re-delivery of delivered — a re-plan from the reset cursor never
        //     touches t0/t1 (they're gone from the compacted file).
        let plan = plan_round(&kept, 0, |_| true);
        let replanned: Vec<&str> = plan
            .iter()
            .filter_map(|d| match d {
                Decision::Deliver { tab, .. } => Some(tab.as_str()),
                Decision::Skip { .. } => None,
            })
            .collect();
        assert_eq!(replanned, vec!["t2", "t3"], "only the undelivered are re-planned");
        assert!(
            !replanned.contains(&"t0") && !replanned.contains(&"t1"),
            "delivered entries are never re-delivered after compaction"
        );
        // (c) coherent cursor — cursor 0 on the shortened file addresses the
        //     first undelivered entry (t2); no off-by-one, nothing skipped.
        assert_eq!(clamp_cursor(0, kept.len()), 0);
        assert_eq!(kept.first().map(|e| e.tab.as_str()), Some("t2"));
    }

    #[test]
    fn compact_of_fully_drained_round_is_empty_and_clamps() {
        let entries = vec![entry("t0", "a", true), entry("t1", "b", true)];
        // Everything delivered (cursor == len) → the compacted file is empty.
        assert!(compact(&entries, 2).is_empty());
        // A stale over-the-end cursor clamps instead of panicking on the slice.
        assert!(compact(&entries, 99).is_empty());
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
        assert!(!is_deliverable(&tabs, "claude-nosession"));
        assert!(!is_deliverable(&tabs, "shell"));
        assert!(!is_deliverable(&tabs, "catbus"));
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
        assert!(e[1].submit); // defaults true when omitted
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
        assert_eq!(parse_swamp(&line), vec![e]);
    }

    #[test]
    fn cursor_clamps_when_swamp_shrinks() {
        assert_eq!(clamp_cursor(9, 3), 3);
        assert_eq!(clamp_cursor(2, 3), 2);
        assert_eq!(clamp_cursor(0, 0), 0);
    }

    #[test]
    fn parse_run_opts_covers_every_branch() {
        // Defaults.
        assert_eq!(
            parse_run_opts(&[]).unwrap(),
            RunOpts {
                once: false,
                interval: DEFAULT_INTERVAL_SECS
            }
        );
        // --once + --interval.
        assert_eq!(
            parse_run_opts(&["--once".into(), "--interval".into(), "9".into()]).unwrap(),
            RunOpts {
                once: true,
                interval: 9
            }
        );
        // Bad interval (zero / non-numeric / missing) → exit 2.
        assert_eq!(parse_run_opts(&["--interval".into(), "0".into()]), Err(2));
        assert_eq!(parse_run_opts(&["--interval".into(), "x".into()]), Err(2));
        assert_eq!(parse_run_opts(&["--interval".into()]), Err(2));
        // Help → exit 0; unknown → exit 2.
        assert_eq!(parse_run_opts(&["--help".into()]), Err(0));
        assert_eq!(parse_run_opts(&["--nope".into()]), Err(2));
    }

    #[test]
    fn parse_swamp_args_covers_every_branch() {
        // Minimal: tab + text, submit defaults true.
        assert_eq!(
            parse_swamp_args(&["uuid".into(), "hello".into()]).unwrap(),
            SwampArgs {
                tab: "uuid".into(),
                input: "hello".into(),
                submit: true,
                from: None,
            }
        );
        // Flags in any position.
        assert_eq!(
            parse_swamp_args(&[
                "--no-submit".into(),
                "uuid".into(),
                "hi".into(),
                "--from".into(),
                "bot".into()
            ])
            .unwrap(),
            SwampArgs {
                tab: "uuid".into(),
                input: "hi".into(),
                submit: false,
                from: Some("bot".into()),
            }
        );
        // Missing text → 2; extra positional → 2; help → 0.
        assert_eq!(parse_swamp_args(&["only-tab".into()]), Err(2));
        assert_eq!(parse_swamp_args(&["a".into(), "b".into(), "c".into()]), Err(2));
        assert_eq!(parse_swamp_args(&["-h".into()]), Err(0));
    }

    #[test]
    fn plan_round_delivers_from_cursor_and_guards_targets() {
        let entries = vec![
            entry("old", "done", true),       // 0: before the cursor
            entry("claude", "go", true),      // 1: deliverable
            entry("shell", "rm -rf /", true), // 2: guard trips
            entry("claude", "again", false),  // 3: deliverable, no submit
        ];
        let ok = |uuid: &str| uuid == "claude";
        let plan = plan_round(&entries, 1, ok);
        assert_eq!(
            plan,
            vec![
                Decision::Deliver {
                    index: 1,
                    tab: "claude".into(),
                    input: "go".into(),
                    submit: true
                },
                Decision::Skip {
                    index: 2,
                    tab: "shell".into()
                },
                Decision::Deliver {
                    index: 3,
                    tab: "claude".into(),
                    input: "again".into(),
                    submit: false
                },
            ]
        );
        // Cursor past the end → empty plan, no panic.
        assert!(plan_round(&entries, 99, ok).is_empty());
    }

    #[test]
    fn append_swamp_line_appends_parseable_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("swamp.jsonl");
        append_swamp_line(&path, &entry("t1", "one", true)).unwrap();
        append_swamp_line(&path, &entry("t2", "two", false)).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_swamp(&body);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].tab, "t1");
        assert_eq!(parsed[1].tab, "t2");
        assert!(!parsed[1].submit);
    }
}
