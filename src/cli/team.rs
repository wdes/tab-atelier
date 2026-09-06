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
    /// `#[serde(default)]` alone is not enough: it covers a MISSING field, and
    /// `/tabs` sends `"cwd": null` for a tab whose cwd isn't known yet (fresh
    /// tab, /proc read failed). Without this the whole listing fails to parse
    /// and `peers` reports an error for the entire fleet.
    #[serde(default, deserialize_with = "null_to_default")]
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
/// Deserialize `null` as `T::default()` rather than failing. One tab missing
/// an optional field must not sink the whole `/tabs` listing.
fn null_to_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
                let Some(v) = it.next() else {
                    eprintln!("peek: --lines needs a value");
                    return 2;
                };
                let Ok(n) = v.parse::<usize>() else {
                    eprintln!("peek: --lines must be a whole number");
                    return 2;
                };
                lines = n;
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
    /// Stable identity, unique across hosts. This is what makes the log a
    /// grow-only set: merging two blackboards is a union keyed by `id`, which
    /// is idempotent, commutative and associative — so hosts converge without
    /// agreeing on anything (Shapiro et al., CRDTs, 2011).
    ///
    /// Empty on entries written before typed notes existed; [`parse_notes`]
    /// backfills those from a content hash, so old lines still de-duplicate.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Which host wrote it, for display and for gossip accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// What kind of entry this is. Plain notes stay `note`, so every existing
    /// line keeps its meaning.
    #[serde(default, skip_serializing_if = "NoteKind::is_note")]
    pub kind: NoteKind,
    /// The task this entry concerns (`announce`/`bid`/`award`/`done`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// A bid's asking price — lower wins. Units are the fleet's business; what
    /// matters is that they compare.
    ///
    /// An integer on purpose: a NaN loose in a bid ordering would make the
    /// winner depend on comparison order, which is precisely the kind of
    /// non-determinism a convergent fold must not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<i64>,
    /// An award's winner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Whether a `done` reports success. `None` on other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

/// The contract-net message types (Smith, 1980), carried on the blackboard
/// rather than on a new transport: announce a task, bid for it, award it,
/// report completion.
///
/// Keeping them as entries on the existing append-only log means the whole
/// protocol inherits the log's properties for free — durable, readable by
/// every tab, and mergeable across hosts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    /// A plain broadcast — what `note` has always written.
    #[default]
    Note,
    /// Work that wants doing.
    Announce,
    /// "I could take that, at this cost."
    Bid,
    /// "It is yours."
    Award,
    /// "Finished" (or "failed", per `ok`) — the explicit termination signal
    /// that silence-polling can only guess at.
    Done,
}

impl NoteKind {
    /// Plain notes are the serialisation default, so they stay off the wire.
    #[must_use]
    pub const fn is_note(&self) -> bool {
        matches!(self, Self::Note)
    }

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Announce => "announce",
            Self::Bid => "bid",
            Self::Award => "award",
            Self::Done => "done",
        }
    }

    /// Parse a CLI/wire spelling.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "note" => Some(Self::Note),
            "announce" => Some(Self::Announce),
            "bid" => Some(Self::Bid),
            "award" => Some(Self::Award),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// FNV-1a over a string. Stable across processes and hosts (unlike
/// `DefaultHasher`), which is what matters for deriving entry ids and for
/// rendezvous ranking.
#[must_use]
pub fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Mint an entry id: origin + time + a hash of the content, so two hosts
/// writing at the same millisecond still differ, and a replayed line keeps its
/// identity rather than duplicating on merge.
#[must_use]
pub fn mint_id(origin: &str, ts: u64, content: &str) -> String {
    format!("{origin}-{ts:x}-{:016x}", stable_hash(content))
}

/// This instance's identity in the fleet: `<hostname>-<8 hex>`, minted once
/// and kept in the state directory.
///
/// The hostname alone is not enough, and the sandbox test proved it: two
/// instances on one machine both called themselves the same thing, so each
/// believed the other's tasks were its own — a task's home host could never be
/// resolved, and federated claims silently degraded to local ones. Cloned VMs
/// and default hostnames have the same problem in the field.
///
/// It is worse than a routing bug. Entry ids are minted from the origin, so
/// two instances sharing one could mint the *same id* for different entries
/// written in the same second — and a grow-only set de-duplicates by id, so
/// one of them would vanish on merge. The suffix makes ids unique per state
/// directory, which is the granularity that actually matters.
#[must_use]
pub fn origin_id() -> String {
    if let Some(cached) = ORIGIN.get() {
        return cached.clone();
    }
    // Only the default location is cached: a test that redirects the board
    // must not inherit an identity minted for another one.
    let default_path = BLACKBOARD_OVERRIDE.read().ok().and_then(|g| g.clone()).is_none();
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".to_owned());
    // Beside the blackboard it stamps, so a test pointing the board at a
    // tempdir gets a temp identity too rather than writing to the developer's
    // real state directory. Same location in production.
    let path = blackboard_path().with_file_name("origin");
    let suffix = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Uniqueness, not unpredictability: this names an instance, it does
            // not authenticate one.
            let seed = format!(
                "{host}-{}-{}-{}",
                crate::unix_millis(),
                std::process::id(),
                blackboard_path().display()
            );
            let minted = format!("{:08x}", stable_hash(&seed) & 0xffff_ffff);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, &minted);
            minted
        });
    let id = format!("{host}-{suffix}");
    if default_path {
        let _ = ORIGIN.set(id.clone());
    }
    id
}

/// Cached so the file is read once per process — `origin_id` is called on
/// every board write.
static ORIGIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Every entry on this host's blackboard, oldest first.
///
/// Shared with the API so `GET /blackboard` and the CLI read exactly the same
/// file — the log stays the single source of truth rather than being mirrored
/// into daemon state that could drift from it.
#[must_use]
pub fn read_blackboard() -> Vec<Note> {
    parse_notes(&std::fs::read_to_string(blackboard_path()).unwrap_or_default())
}

/// Append the entries of `incoming` we don't already have, returning how many
/// were added.
///
/// The union is what makes gossip safe to run in any pattern: re-merging a
/// batch adds nothing, and two hosts merging each other's logs converge
/// regardless of who goes first.
///
/// # Errors
/// When the blackboard file can't be created or appended to.
pub fn merge_into_blackboard(incoming: &[Note]) -> Result<usize, String> {
    use std::io::Write as _;
    let path = blackboard_path();
    let have = read_blackboard();
    let new = merge_notes(&have, incoming);
    if new.is_empty() {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut body = String::new();
    for n in &new {
        body.push_str(&encode_note_line(n));
    }
    f.write_all(body.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(new.len())
}

/// Append one entry, whatever its kind. `note` is this with `kind: Note`.
///
/// # Errors
/// When the blackboard file can't be created or appended to.
pub fn append_entry(mut n: Note) -> Result<Note, String> {
    use std::io::Write as _;
    let origin = origin_id();
    if n.id.is_empty() {
        n.id = mint_id(
            &origin,
            n.ts,
            &format!("{:?}{:?}{}{:?}{:?}", n.from, n.task, n.msg, n.kind, n.cost),
        );
    }
    if n.origin.is_none() {
        n.origin = Some(origin);
    }
    let path = blackboard_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(encode_note_line(&n).as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(n)
}

/// Build an entry with the current time and this host's origin filled in.
#[must_use]
pub fn new_entry(kind: NoteKind, from: Option<String>, msg: &str) -> Note {
    Note {
        ts: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        from,
        topic: None,
        msg: msg.to_owned(),
        id: String::new(),
        origin: None,
        kind,
        task: None,
        cost: None,
        to: None,
        ok: None,
    }
}

/// Serialises every test that redirects the process-global blackboard or
/// lease-registry path.
///
/// One lock, shared: three helpers used to hold three separate mutexes while
/// pointing the same global at their own tempdir, so they took each other's
/// board mid-assert and failed at random in a full run.
#[cfg(test)]
pub static BOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test/ops override for the blackboard file. Set via
/// [`set_blackboard_path`].
static BLACKBOARD_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Point the blackboard at a different file. Tests use a tempdir so a run
/// never appends to the developer's real board; `None` restores the default.
pub fn set_blackboard_path(path: Option<PathBuf>) {
    if let Ok(mut g) = BLACKBOARD_OVERRIDE.write() {
        *g = path;
    }
}

pub(crate) fn blackboard_path() -> PathBuf {
    if let Some(p) = BLACKBOARD_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return p;
    }
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
        .filter_map(|l| {
            serde_json::from_str::<Note>(l).ok().map(|mut n| {
                if n.id.is_empty() {
                    // Pre-typed entries carry no id. Deriving one from the content
                    // keeps merge idempotent: the same old line seen twice is one
                    // entry, not two.
                    n.id = mint_id("legacy", n.ts, &format!("{:?}{:?}{}", n.from, n.topic, n.msg));
                }
                n
            })
        })
        .collect()
}

/// Merge `incoming` into `have`, returning the entries that were new.
///
/// This is the whole cross-host story: a union keyed by id. Idempotent (the
/// same batch twice adds nothing), commutative and associative (order and
/// grouping don't matter), so two daemons that gossip in any pattern converge
/// on the same blackboard without agreeing on anything first.
#[must_use]
pub fn merge_notes(have: &[Note], incoming: &[Note]) -> Vec<Note> {
    let known: std::collections::HashSet<&str> = have.iter().map(|n| n.id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    incoming
        .iter()
        .filter(|n| !n.id.is_empty() && !known.contains(n.id.as_str()) && seen.insert(n.id.clone()))
        .cloned()
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
            "--since" => {
                let Some(v) = it.next() else {
                    eprintln!("notes: --since needs a value");
                    return 2;
                };
                let Ok(n) = v.parse::<usize>() else {
                    eprintln!("notes: --since must be a whole number");
                    return 2;
                };
                since = Some(n);
            }
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
    note_at(&blackboard_path(), topic, from, msg)
}

/// [`note`] against an explicit blackboard file.
#[must_use]
fn note_at(path: &Path, topic: Option<String>, from: Option<String>, msg: &str) -> i32 {
    use std::io::Write as _;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let origin = origin_id();
    let n = Note {
        id: mint_id(&origin, ts, &format!("{from:?}{topic:?}{msg}")),
        origin: Some(origin),
        ts,
        from,
        topic,
        msg: msg.to_string(),
        kind: NoteKind::Note,
        task: None,
        cost: None,
        to: None,
        ok: None,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append mode: concurrent small writes from many tabs stay line-atomic.
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
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
    notes_at(&blackboard_path(), topic, since)
}

/// [`notes`] against an explicit blackboard file.
#[must_use]
fn notes_at(path: &Path, topic: Option<&str>, since: Option<usize>) -> i32 {
    let body = std::fs::read_to_string(path).unwrap_or_default();
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

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_tab_without_a_cwd_does_not_sink_the_whole_listing() {
        // Regression: `/tabs` sends `"cwd": null` for a tab whose cwd isn't
        // known yet, and serde's `default` only covers a MISSING key — so one
        // such tab made `peers` fail for every tab.
        let v: TabView =
            serde_json::from_str(r#"{"index":0,"id":"a","name":"n","cwd":null}"#).expect("null cwd parses");
        assert_eq!(v.cwd, "");
        let v: TabView = serde_json::from_str(r#"{"index":0,"id":"a","name":"n"}"#).expect("missing cwd parses");
        assert_eq!(v.cwd, "");
    }

    #[test]
    fn the_blackboard_round_trips_through_a_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("blackboard.jsonl");
        assert_eq!(
            note_at(&path, Some("schema".into()), Some("api".into()), "users.email NOT NULL"),
            0
        );
        assert_eq!(note_at(&path, None, None, "untopiced"), 0);
        let body = std::fs::read_to_string(&path).expect("read");
        let all = parse_notes(&body);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].topic.as_deref(), Some("schema"));
        assert_eq!(all[0].from.as_deref(), Some("api"));
        assert!(all[0].ts > 0, "stamped at write");
        // Reading is filtered, never destructive.
        assert_eq!(notes_at(&path, None, None), 0);
        assert_eq!(notes_at(&path, Some("schema"), None), 0);
        assert_eq!(notes_at(&path, Some("nothing-here"), None), 0);
        assert_eq!(notes_at(&path, None, Some(1)), 0, "--since skips read entries");
        assert_eq!(parse_notes(&std::fs::read_to_string(&path).expect("read")).len(), 2);
        // A missing file reads as empty rather than failing.
        assert_eq!(notes_at(&tmp.path().join("absent.jsonl"), None, None), 0);
        // The first note creates the file (and its parent) on demand.
        let nested = tmp.path().join("deep/blackboard.jsonl");
        assert_eq!(note_at(&nested, None, None, "hi"), 0);
        assert!(nested.exists());
    }

    #[test]
    fn note_and_notes_parse_their_flags() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("b.jsonl");
        assert_eq!(note_at(&path, None, None, "msg"), 0);
        // A message is required; flags alone are usage.
        assert_eq!(run_note(&argv(&[])), 2);
        assert_eq!(run_note(&argv(&["--topic", "t"])), 2);
        // --since must be a number, or a typo would silently show everything.
        assert_eq!(run_notes(&argv(&["--since", "abc"])), 2);
        assert_eq!(run_notes(&argv(&["--since"])), 2);
    }

    #[test]
    fn peer_and_peek_verbs_read_the_live_fleet() {
        crate::cli::share_link::with_test_server(|_| {
            assert_eq!(peers(false), 0);
            assert_eq!(peers(true), 0, "--all");
            assert_eq!(peek("0", 5, false), 0);
            assert_eq!(peek("tab-b", 5, true), 0, "--raw");
            assert_eq!(peek("nope", 5, false), 1);
            assert_eq!(run_peek(&argv(&["0"])), 0);
            assert_eq!(run_peek(&argv(&["0", "--lines", "3"])), 0);
            assert_eq!(run_peek(&argv(&[])), 2, "a tab is required");
            assert_eq!(run_peek(&argv(&["0", "--lines", "abc"])), 2);
        });
    }

    #[test]
    fn handoff_needs_a_real_file_and_a_real_tab() {
        crate::cli::share_link::with_test_server(|_| {
            let tmp = tempfile::tempdir().expect("tmp");
            let f = tmp.path().join("report.md");
            std::fs::write(&f, "hello").expect("write");
            // The target tab must exist…
            assert_eq!(handoff(&f, "nope"), 1);
            // …and so must the file, before anything is uploaded.
            assert_eq!(handoff(&tmp.path().join("absent.md"), "0"), 1);
            assert_eq!(run_handoff(&argv(&[])), 2);
            assert_eq!(run_handoff(&argv(&["only-one-arg"])), 2);
        });
    }

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

    fn note(ts: u64, topic: Option<&str>, from: Option<&str>, msg: &str) -> Note {
        Note {
            ts,
            topic: topic.map(Into::into),
            from: from.map(Into::into),
            msg: msg.into(),
            id: format!("t-{ts}"),
            origin: None,
            kind: NoteKind::Note,
            task: None,
            cost: None,
            to: None,
            ok: None,
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
