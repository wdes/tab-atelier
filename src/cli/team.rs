// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Claude-to-Claude teamwork verbs over the local API.
//!
//! NOT the catbus agent framework (which coordinates its own agents) — these
//! are thin, session-safe wrappers so the plain `claude` tabs can see,
//! broadcast to, and hand files to each other:
//!
//!   - `peers`             — list sibling tabs (name / state / cwd / context)
//!   - `note` / `notes`    — an append-only shared blackboard all tabs can read
//!   - `handoff`           — drop a file into a peer tab's `inbox/`
//!
//! Sending a prompt to another agent and waiting for its answer already lives
//! in `tab-atelier dispatch` (see `cli::delegate`); this module is the rest.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cli::share_link::{Endpoint, discover_endpoint, fetch_tabs};

/// The subset of a `/tabs` entry the team commands care about. Deserialised
/// from the JSON `fetch_tabs` returns.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TabView {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    /// "thinking" | "waiting" | "error" | absent. Absent ⇒ idle at a prompt.
    #[serde(default)]
    pub agent_state: Option<String>,
    /// "claude" | "catbus" | absent. Only Claude tabs are teammates.
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub locked: bool,
}

impl TabView {
    /// Human-facing status word for the LED state (`None` ⇒ "idle").
    #[must_use]
    pub fn state_word(&self) -> &str {
        match self.agent_state.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => "idle",
        }
    }
}

/// Fetch `/tabs` and deserialise into typed [`TabView`]s.
pub(crate) fn fetch_tab_views(ep: &Endpoint) -> Result<Vec<TabView>, String> {
    let raw = fetch_tabs(ep)?;
    raw.into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| format!("parse tab: {e}")))
        .collect()
}

/// Pick the tabs to show as teammates: Claude sessions only, unless `all`.
/// A Claude tab is one whose `agent_kind` is `"claude"`.
#[must_use]
pub fn select_peers(tabs: &[TabView], all: bool) -> Vec<&TabView> {
    tabs.iter()
        .filter(|t| all || t.agent_kind.as_deref() == Some("claude"))
        .collect()
}

/// One `peers` line: `[idx] name · state · cwd — context`.
#[must_use]
pub fn format_peer_line(t: &TabView) -> String {
    let lock = if t.locked { " 🔒" } else { "" };
    let ctx = match t.context.as_deref() {
        Some(c) if !c.is_empty() => format!(" — {c}"),
        _ => String::new(),
    };
    format!("[{}] {}{lock} · {} · {}{ctx}", t.index, t.name, t.state_word(), t.cwd)
}

/// `tab-atelier peers [--all]` — list sibling tabs so a Claude can pick a
/// collaborator (or wait on one) by name.
#[must_use]
pub fn peers(all: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("peers: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tab_views(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("peers: {e}");
            return 1;
        }
    };
    let sel = select_peers(&tabs, all);
    if sel.is_empty() {
        println!("(no {} tabs)", if all { "" } else { "Claude " });
        return 0;
    }
    for t in sel {
        println!("{}", format_peer_line(t));
    }
    0
}

/// One `tabs` line: `<idx>  <uuid>  <name>` — UUID-first so it copy-pastes
/// straight into `dispatch --to <uuid>` or an API path.
#[must_use]
pub fn format_tab_line(t: &TabView) -> String {
    format!("{:>3}  {}  {}", t.index, t.id, t.name)
}

/// List every tab as `idx  uuid  name`.
///
/// Unlike `peers` (Claude teammates only), this shows all tabs. A client
/// command: it talks to the running instance over the local API, so it never
/// trips the single-instance lock.
#[must_use]
pub fn tabs() -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tabs: {e}");
            return 1;
        }
    };
    let views = match fetch_tab_views(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tabs: {e}");
            return 1;
        }
    };
    for t in &views {
        println!("{}", format_tab_line(t));
    }
    0
}

/// CLI entry for `peek <tab> [--lines N] [--raw]`. Parses the flags the
/// hand-rolled GUI dispatcher can't model with clap, then calls [`peek`].
#[must_use]
pub fn run_peek(args: &[String]) -> i32 {
    let mut tab: Option<&str> = None;
    let mut lines: usize = 40;
    let mut raw = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--raw" => raw = true,
            "--lines" => {
                if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                    lines = n;
                }
            }
            other if tab.is_none() => tab = Some(other),
            _ => {}
        }
    }
    tab.map_or_else(
        || {
            eprintln!("peek: usage: tab-atelier peek <tab> [--lines N] [--raw]");
            2
        },
        |t| peek(t, lines, raw),
    )
}

/// Resolve a target key to exactly one tab.
///
/// Tries, in order: exact name, then index, then UUID. An ambiguous name (more
/// than one tab shares it) is an error listing the indexes, so a message never
/// silently goes to the wrong twin.
///
/// # Errors
/// When no tab matches `key`, or when several tabs share the name `key` (which
/// index to use is then the caller's to disambiguate).
pub fn resolve_target<'a>(tabs: &'a [TabView], key: &str) -> Result<&'a TabView, String> {
    let named: Vec<&TabView> = tabs.iter().filter(|t| t.name == key).collect();
    match named.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => {
            let idxs = many.iter().map(|t| t.index.to_string()).collect::<Vec<_>>().join(", ");
            return Err(format!(
                "{} tabs named {key:?} (indexes {idxs}); address by index",
                many.len()
            ));
        }
    }
    if let Ok(idx) = key.parse::<usize>()
        && let Some(t) = tabs.iter().find(|t| t.index == idx)
    {
        return Ok(t);
    }
    if let Some(t) = tabs.iter().find(|t| t.id == key) {
        return Ok(t);
    }
    Err(format!("no tab matches {key:?}"))
}

// --- peek (`peek`) -------------------------------------------------------

/// The last `n` lines of `text` (all of it when it has fewer), joined with
/// `\n`. Trailing blank lines are dropped first so a screen padded to the
/// window height doesn't spend the budget on emptiness.
#[must_use]
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// `tab-atelier peek <tab> [--lines N] [--raw]` — read a peer tab's screen.
///
/// ANSI-stripped (unless `--raw`), last `N` lines. The ergonomic read primitive
/// agents otherwise hand-roll (name-addressed, token auto-discovered).
#[must_use]
pub fn peek(tab: &str, lines: usize, raw: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("peek: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tab_views(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("peek: {e}");
            return 1;
        }
    };
    let target = match resolve_target(&tabs, tab) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("peek: {e}");
            return 1;
        }
    };
    let body = match crate::cli::share_link::agent()
        .get(format!("{}/tabs/by-id/{}/output", ep.url, target.id))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("peek: read {}: {e}", target.name);
            return 1;
        }
    };
    let text = if raw { body } else { crate::strip_ansi(&body) };
    println!("{}", tail_lines(&text, lines));
    0
}

// --- blackboard (`note` / `notes`) ---------------------------------------

/// One shared-blackboard entry.
///
/// Persisted as one JSON line in `<state>/tab-atelier/blackboard.jsonl` — an
/// append-only log every tab reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Note {
    /// Unix seconds when posted.
    pub ts: u64,
    /// Who posted it (a tab name), if given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Optional channel so readers can filter (`--topic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub msg: String,
}

fn blackboard_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join("tab-atelier")
        .join("blackboard.jsonl")
}

/// One note as a JSONL line (trailing newline included). Never panics — the
/// crate forbids unwrap/expect, and this shape always serialises anyway.
#[must_use]
pub fn encode_note_line(n: &Note) -> String {
    serde_json::to_string(n).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// Parse a blackboard body into notes, skipping blank / unparseable lines (a
/// half-written line from a racing appender is dropped, not fatal).
#[must_use]
pub fn parse_notes(body: &str) -> Vec<Note> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Note>(l).ok())
        .collect()
}

/// Notes matching `topic` (None = all) whose position in the FULL list is
/// `>= since`.
///
/// The index is the position in the full log — stable regardless of the topic
/// filter — so `--since <n>` polls incrementally without a topic shifting the
/// numbering.
#[must_use]
pub fn select_notes<'a>(notes: &'a [Note], topic: Option<&str>, since: usize) -> Vec<(usize, &'a Note)> {
    notes
        .iter()
        .enumerate()
        .filter(|(i, n)| *i >= since && topic.is_none_or(|t| n.topic.as_deref() == Some(t)))
        .collect()
}

/// One `notes` line: `#idx [topic] from: msg` (topic/from omitted when absent).
#[must_use]
pub fn format_note(idx: usize, n: &Note) -> String {
    let topic = n.topic.as_deref().map_or_else(String::new, |t| format!("[{t}] "));
    let from = n.from.as_deref().map_or_else(String::new, |f| format!("{f}: "));
    format!("#{idx} {topic}{from}{}", n.msg)
}

/// CLI entry for `note [--topic T] [--from NAME] <msg>` — parses the flags the
/// GUI binary's hand-rolled dispatcher needs, then calls [`note`].
#[must_use]
pub fn run_note(args: &[String]) -> i32 {
    let mut topic: Option<String> = None;
    let mut from: Option<String> = None;
    let mut msg: Option<&str> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--topic" => topic = it.next().cloned(),
            "--from" => from = it.next().cloned(),
            other if msg.is_none() => msg = Some(other),
            _ => {}
        }
    }
    msg.map_or_else(
        || {
            eprintln!("note: usage: tab-atelier note [--topic T] [--from NAME] <msg>");
            2
        },
        |m| note(topic, from, m),
    )
}

/// CLI entry for `notes [--topic T] [--since N]` — parses flags, calls [`notes`].
#[must_use]
pub fn run_notes(args: &[String]) -> i32 {
    let mut topic: Option<&str> = None;
    let mut since: Option<usize> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--topic" => topic = it.next().map(String::as_str),
            "--since" => since = it.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    notes(topic, since)
}

/// CLI entry for `handoff <file> <tab>` — calls [`handoff`].
#[must_use]
pub fn run_handoff(args: &[String]) -> i32 {
    if let [file, tab, ..] = args {
        handoff(Path::new(file), tab)
    } else {
        eprintln!("handoff: usage: tab-atelier handoff <file> <tab>");
        2
    }
}

/// `tab-atelier note [--topic T] [--from NAME] <msg>` — post to the blackboard.
#[must_use]
pub fn note(topic: Option<String>, from: Option<String>, msg: &str) -> i32 {
    use std::io::Write as _;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let n = Note {
        ts,
        from,
        topic,
        msg: msg.to_string(),
    };
    let path = blackboard_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append mode: concurrent small writes from many tabs stay line-atomic.
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(encode_note_line(&n).as_bytes()) {
                eprintln!("note: write {}: {e}", path.display());
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("note: open {}: {e}", path.display());
            1
        }
    }
}

/// `tab-atelier notes [--topic T] [--since N]` — read the blackboard.
#[must_use]
pub fn notes(topic: Option<&str>, since: Option<usize>) -> i32 {
    let path = blackboard_path();
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let all = parse_notes(&body);
    let sel = select_notes(&all, topic, since.unwrap_or(0));
    if sel.is_empty() {
        println!("(no notes)");
        return 0;
    }
    for (i, n) in sel {
        println!("{}", format_note(i, n));
    }
    0
}

// --- file handoff (`handoff`) --------------------------------------------

/// Where a handed-off file lands: the target tab's `inbox/<basename>`. Mirrors
/// the upload route (`api.rs`: files land in `<tab cwd>/inbox`).
///
/// # Errors
/// When `file` has no final component (e.g. it ends in `..` or `/`), so there's
/// no basename to place under `inbox/`.
pub fn inbox_dest(cwd: &Path, file: &Path) -> Result<PathBuf, String> {
    let name = file
        .file_name()
        .ok_or_else(|| format!("{} has no file name", file.display()))?;
    Ok(cwd.join("inbox").join(name))
}

/// `tab-atelier handoff <file> <tab>` — copy a file into a peer tab's `inbox/`
/// so its agent can pick it up (drag the path into Claude, or poll the dir).
#[must_use]
pub fn handoff(file: &Path, tab: &str) -> i32 {
    if !file.is_file() {
        eprintln!("handoff: {} is not a readable file", file.display());
        return 1;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("handoff: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tab_views(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("handoff: {e}");
            return 1;
        }
    };
    let target = match resolve_target(&tabs, tab) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("handoff: {e}");
            return 1;
        }
    };
    if target.cwd.is_empty() {
        eprintln!("handoff: tab {:?} has no cwd", target.name);
        return 1;
    }
    let dest = match inbox_dest(Path::new(&target.cwd), file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("handoff: {e}");
            return 1;
        }
    };
    if let Some(parent) = dest.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("handoff: mkdir {}: {e}", parent.display());
        return 1;
    }
    if let Err(e) = std::fs::copy(file, &dest) {
        eprintln!("handoff: copy → {}: {e}", dest.display());
        return 1;
    }
    println!("handed {} → {} ({})", file.display(), dest.display(), target.name);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(index: usize, name: &str, kind: Option<&str>, state: Option<&str>) -> TabView {
        TabView {
            index,
            id: format!("id-{index}"),
            name: name.into(),
            cwd: format!("/w/{name}"),
            agent_state: state.map(Into::into),
            agent_kind: kind.map(Into::into),
            agent_session_id: None,
            context: None,
            locked: false,
        }
    }

    #[test]
    fn select_peers_filters_to_claude_unless_all() {
        let tabs = vec![
            tab(0, "a", Some("claude"), None),
            tab(1, "sh", None, None),
            tab(2, "cb", Some("catbus"), None),
        ];
        let claude = select_peers(&tabs, false);
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].name, "a");
        // --all keeps everything, order preserved.
        let all = select_peers(&tabs, true);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn state_word_maps_none_to_idle() {
        assert_eq!(tab(0, "a", Some("claude"), None).state_word(), "idle");
        assert_eq!(tab(0, "a", Some("claude"), Some("")).state_word(), "idle");
        assert_eq!(tab(0, "a", Some("claude"), Some("thinking")).state_word(), "thinking");
    }

    #[test]
    fn format_peer_line_shows_index_name_state_cwd_and_context() {
        let mut t = tab(3, "db", Some("claude"), Some("thinking"));
        t.context = Some("migrations".into());
        assert_eq!(format_peer_line(&t), "[3] db · thinking · /w/db — migrations");
        // No context, locked → lock marker, no trailing dash.
        let mut l = tab(4, "ops", Some("claude"), None);
        l.locked = true;
        assert_eq!(format_peer_line(&l), "[4] ops 🔒 · idle · /w/ops");
    }

    #[test]
    fn format_tab_line_is_index_uuid_name() {
        assert_eq!(format_tab_line(&tab(7, "deb", None, None)), "  7  id-7  deb");
    }

    fn note(ts: u64, topic: Option<&str>, from: Option<&str>, msg: &str) -> Note {
        Note {
            ts,
            topic: topic.map(Into::into),
            from: from.map(Into::into),
            msg: msg.into(),
        }
    }

    #[test]
    fn parse_notes_skips_blank_and_broken_lines() {
        let body = format!(
            "{}\n\n  \nnot json\n{}\n",
            encode_note_line(&note(1, Some("db"), Some("a"), "hi")).trim_end(),
            encode_note_line(&note(2, None, None, "yo")).trim_end(),
        );
        let n = parse_notes(&body);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].msg, "hi");
        assert_eq!(n[1].topic, None);
    }

    #[test]
    fn encode_then_parse_roundtrips() {
        let n = note(42, Some("t"), Some("f"), "message");
        let parsed = parse_notes(&encode_note_line(&n));
        assert_eq!(parsed, vec![n]);
    }

    #[test]
    fn select_notes_filters_by_topic_and_keeps_global_index() {
        let all = vec![
            note(1, Some("db"), None, "a"),
            note(2, Some("net"), None, "b"),
            note(3, Some("db"), None, "c"),
        ];
        // Topic filter keeps the position in the FULL log as the index.
        let db = select_notes(&all, Some("db"), 0);
        assert_eq!(db.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 2]);
        // `since` is measured against the full log, not the filtered view.
        let db_since = select_notes(&all, Some("db"), 1);
        assert_eq!(db_since.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2]);
        // No topic → everything from `since` on.
        assert_eq!(select_notes(&all, None, 2).len(), 1);
    }

    #[test]
    fn format_note_omits_absent_topic_and_from() {
        assert_eq!(format_note(0, &note(1, Some("db"), Some("a"), "hi")), "#0 [db] a: hi");
        assert_eq!(format_note(5, &note(1, None, None, "bare")), "#5 bare");
    }

    #[test]
    fn resolve_target_prefers_name_then_index_then_uuid() {
        let mut tabs = vec![tab(0, "db", Some("claude"), None), tab(1, "web", Some("claude"), None)];
        tabs[1].id = "uuid-web".into();
        assert_eq!(resolve_target(&tabs, "db").unwrap().index, 0);
        assert_eq!(resolve_target(&tabs, "1").unwrap().name, "web");
        assert_eq!(resolve_target(&tabs, "uuid-web").unwrap().index, 1);
        assert!(resolve_target(&tabs, "nope").is_err());
    }

    #[test]
    fn resolve_target_rejects_ambiguous_name() {
        let tabs = vec![
            tab(2, "m-PF", Some("claude"), None),
            tab(5, "m-PF", Some("claude"), None),
        ];
        let err = resolve_target(&tabs, "m-PF").unwrap_err();
        assert!(err.contains("2 tabs named"), "got: {err}");
        assert!(err.contains("2, 5"), "should list indexes: {err}");
    }

    #[test]
    fn tail_lines_keeps_last_n_and_drops_trailing_blanks() {
        let screen = "one\ntwo\nthree\nfour\n\n\n";
        assert_eq!(tail_lines(screen, 2), "three\nfour");
        // Asking for more lines than exist returns all (no padding, no panic).
        assert_eq!(tail_lines("a\nb", 10), "a\nb");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn inbox_dest_is_cwd_inbox_basename() {
        let dest = inbox_dest(Path::new("/mnt/proj"), Path::new("/tmp/report.md")).unwrap();
        assert_eq!(dest, PathBuf::from("/mnt/proj/inbox/report.md"));
        // A path ending in `..` has no file name → error, not a bad dest.
        assert!(inbox_dest(Path::new("/mnt/proj"), Path::new("/tmp/..")).is_err());
    }

    #[test]
    fn tab_view_deserialises_partial_tabs_json() {
        // Only the fields we declared; extras ignored, missing default.
        let v: TabView =
            serde_json::from_str(r#"{"index":2,"id":"u","name":"n","agent_kind":"claude","extra":true}"#).unwrap();
        assert_eq!(v.index, 2);
        assert_eq!(v.name, "n");
        assert_eq!(v.agent_kind.as_deref(), Some("claude"));
        assert!(!v.locked);
        assert_eq!(v.agent_state, None);
    }
}
