// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `decision` — the KIOSK cross-project decision log (PD1).
//!
//! A single EVENT-SOURCED JSONL at `~/.tab-atelier/decisions.jsonl` (outside any repo,
//! survives a restart) collects the PENDING PO decisions of ALL projects
//! (`harness|kalpin|graines`) under one cold source — the native replacement for the
//! silo'd GitHub `#1/#2` digest issues.
//!
//! The engine is the SAME 2-axis fold as the catalogue (`kind:retire|edit|delete|
//! restore`), the pattern copied, not the code:
//! - **CONTENT axis** = latest `open|update` wins (title/why/reco/effort/files).
//! - **STATE axis** = `Archived` if the last `{open|archived}` event is `archived`
//!   (STICKY — a later `read`/`tranched` never un-archives; only an explicit `open`
//!   does), else the last of `{open|read|tranched}` (the open→read→tranched progression).
//!
//! PD1 is CLI + fold ONLY — no UI. The fold is proven by a real-fs rust test BEFORE any
//! panel is built (anti built≠wired). Archiving the actual `files[]` is PD3.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One decision EVENT (append-only).
///
/// `id` groups the events of one decision; `kind` says what the event does; `at`
/// (unix-secs) orders for humans (the FOLD authority is APPEND ORDER, like the
/// catalogue). Content fields ride the `open`/`update` events; `read`/`tranched` carry
/// `by` (+ `verdict`); `archived` carries `archived_path` (PD3).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DecisionEvent {
    pub id: String,
    pub kind: DecisionKind,
    pub at: u64,
    // ----- content (open|update) — skipped when absent so an event stays minimal -----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_gated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reco: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    // ----- state (read|tranched|archived) -----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// PD3: where the bundle was moved on archive. PD1 models the field; the move is PD3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_path: Option<String>,
}

/// The event TYPE. `Open`/`Update` carry content; `Open`/`Read`/`Tranched`/`Archived`
/// drive state. `Retire` has no analogue here — `Open` is the default (a bare line is a
/// new decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionKind {
    /// A new pending decision (content + state=open). The default.
    #[default]
    Open,
    /// A content revision (title/why/reco/effort/files) — does NOT touch state.
    Update,
    /// The PO read it (state → read). Carries `by`.
    Read,
    /// The PO ruled (state → tranched). Carries `verdict` + `by`.
    Tranched,
    /// The bundle was archived (state → archived, STICKY). Carries `archived_path` (PD3).
    Archived,
}

impl DecisionKind {
    /// A CONTENT event — carries the decision's fields (`Open`/`Update`).
    const fn is_content(self) -> bool {
        matches!(self, Self::Open | Self::Update)
    }
}

/// The folded STATE of a decision (STATE axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionState {
    #[default]
    Open,
    Read,
    Tranched,
    Archived,
}

/// One folded decision in the read-model (PD1): the latest content + the derived state.
///
/// Served `rename_all = "camelCase"` (`why_gated` → `whyGated`) so the read-model the
/// KIOSK panel consumes matches the catalogue's camelCase contract (PD2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_gated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reco: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    pub state: DecisionState,
    /// The verdict from the latest `tranched` event, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// The latest content event's timestamp (the fold winner).
    pub at: u64,
}

// ---------------------------------------------------------------------------
// Storage — the cross-project cold source. Append/parse mirror the swamp/catalogue.
// ---------------------------------------------------------------------------

/// The decisions log: `~/.tab-atelier/decisions.jsonl`. Honors
/// `TAB_ATELIER_DECISIONS_PATH` (a test/ops full-path override).
#[must_use]
pub fn decisions_path() -> PathBuf {
    if let Ok(p) = std::env::var("TAB_ATELIER_DECISIONS_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".tab-atelier").join("decisions.jsonl")
}

/// One event as a JSONL line (trailing newline included).
#[must_use]
pub fn encode_line(e: &DecisionEvent) -> String {
    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// Parse a log body into events, skipping blank / unparseable lines (a half-written
/// line from a racing appender is dropped, not fatal — same tolerance as the swamp).
#[must_use]
pub fn parse_decisions(body: &str) -> Vec<DecisionEvent> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<DecisionEvent>(l).ok())
        .collect()
}

/// Append one event to the log (create + append, line-atomic like the swamp / task
/// producer). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / write I/O error.
pub fn append_line(path: &Path, e: &DecisionEvent) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_line(e).as_bytes())
}

// ---------------------------------------------------------------------------
// Fold — 2 axes (content + state), both derived at read. Copy of the catalogue engine.
// ---------------------------------------------------------------------------

/// STATE axis: `Archived` iff the LAST `{Open|Archived}` event is `Archived` (STICKY — a
/// later `read`/`tranched` never un-archives; an explicit `open` does). Otherwise the
/// last of `{Open→Open, Read→Read, Tranched→Tranched}` (the progression). Append order.
#[must_use]
fn state_of(events: &[DecisionEvent]) -> DecisionState {
    let archived = events
        .iter()
        .rev()
        .find(|e| matches!(e.kind, DecisionKind::Open | DecisionKind::Archived))
        .is_some_and(|e| e.kind == DecisionKind::Archived);
    if archived {
        return DecisionState::Archived;
    }
    events
        .iter()
        .rev()
        .find_map(|e| match e.kind {
            DecisionKind::Open => Some(DecisionState::Open),
            DecisionKind::Read => Some(DecisionState::Read),
            DecisionKind::Tranched => Some(DecisionState::Tranched),
            DecisionKind::Update | DecisionKind::Archived => None,
        })
        .unwrap_or_default()
}

/// Fold one decision's events (in APPEND order) into a [`DecisionView`], or `None` when
/// there's no content event (no `open`/`update` → nothing to show).
fn fold_one(id: &str, events: &[DecisionEvent]) -> Option<DecisionView> {
    let content = events.iter().rfind(|e| e.kind.is_content())?;
    // The verdict rides the view ONLY within the CURRENT cycle: surface it iff the
    // latest `Tranched` is AFTER the latest `Open` (append order). A re-`open` starts a
    // fresh cycle, so a pre-archive verdict is stale and MUST NOT ride a re-opened
    // (state=Open) decision — that would show a verdict on an Open item, semantically
    // bancal (Olympe micro-note #2792, verdict-staleness). A never-opened id (bare
    // event) keeps the last tranched verdict.
    let last_open = events.iter().rposition(|e| e.kind == DecisionKind::Open);
    let last_tranched = events.iter().rposition(|e| e.kind == DecisionKind::Tranched);
    let verdict = last_tranched
        .filter(|&t| last_open.is_none_or(|o| t > o))
        .and_then(|t| events[t].verdict.clone());
    Some(DecisionView {
        id: id.to_string(),
        project: content.project.clone(),
        title: content.title.clone(),
        why_gated: content.why_gated.clone(),
        reco: content.reco.clone(),
        effort: content.effort.clone(),
        files: content.files.clone(),
        state: state_of(events),
        verdict,
        at: content.at,
    })
}

/// The read-model over a log file: fold each decision by id, sorted by id.
///
/// `include_archived=false` (the default) HIDES archived decisions; `true` surfaces
/// them (with `state:archived`). A missing file reads empty. READ-ONLY.
#[must_use]
pub fn read_decisions_at(path: &Path, include_archived: bool) -> Vec<DecisionView> {
    use std::collections::BTreeMap;
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let mut by_id: BTreeMap<String, Vec<DecisionEvent>> = BTreeMap::new();
    for e in parse_decisions(&body) {
        by_id.entry(e.id.clone()).or_default().push(e);
    }
    by_id
        .into_iter()
        .filter_map(|(id, events)| fold_one(&id, &events))
        .filter(|v| include_archived || v.state != DecisionState::Archived)
        .collect()
}

/// [`read_decisions_at`] against the live [`decisions_path`]. READ-ONLY.
#[must_use]
pub fn read_decisions(include_archived: bool) -> Vec<DecisionView> {
    read_decisions_at(&decisions_path(), include_archived)
}

// ---------------------------------------------------------------------------
// CLI — `tab-atelier decision push|read|tranch|list [--includeArchived]`.
// ---------------------------------------------------------------------------

fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// `tab-atelier decision <push|read|tranch|list>` (PD1). Append-only mutations +
/// the folded read-model. No UI.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("push") => push(&args[1..]),
        Some("read") => mark(&args[1..], DecisionKind::Read),
        Some("tranch") => mark(&args[1..], DecisionKind::Tranched),
        Some("list") => list(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 tab-atelier decision push --id <id> --project <p> --title <t> [--why <w>] [--reco <r>] [--effort <e>] [--files a,b]\n  \
                 tab-atelier decision read --id <id> [--by <who>]\n  \
                 tab-atelier decision tranch --id <id> --verdict <v> [--by <who>]\n  \
                 tab-atelier decision list [--includeArchived]"
            );
            2
        }
    }
}

fn push(args: &[String]) -> i32 {
    let Some(id) = arg_after(args, "--id").filter(|s| !s.trim().is_empty()) else {
        eprintln!("decision push: --id is required");
        return 2;
    };
    let files = arg_after(args, "--files")
        .map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let e = DecisionEvent {
        id: id.to_string(),
        kind: DecisionKind::Open,
        at: crate::unix_millis() / 1000,
        project: arg_after(args, "--project").map(str::to_string),
        title: arg_after(args, "--title").map(str::to_string),
        why_gated: arg_after(args, "--why").map(str::to_string),
        reco: arg_after(args, "--reco").map(str::to_string),
        effort: arg_after(args, "--effort").map(str::to_string),
        files,
        ..Default::default()
    };
    append_or_report("push", &e)
}

fn mark(args: &[String], kind: DecisionKind) -> i32 {
    let Some(id) = arg_after(args, "--id").filter(|s| !s.trim().is_empty()) else {
        eprintln!("decision: --id is required");
        return 2;
    };
    if kind == DecisionKind::Tranched && arg_after(args, "--verdict").is_none() {
        eprintln!("decision tranch: --verdict is required");
        return 2;
    }
    let e = DecisionEvent {
        id: id.to_string(),
        kind,
        at: crate::unix_millis() / 1000,
        by: arg_after(args, "--by").map(str::to_string),
        verdict: arg_after(args, "--verdict").map(str::to_string),
        ..Default::default()
    };
    append_or_report(if kind == DecisionKind::Read { "read" } else { "tranch" }, &e)
}

fn append_or_report(verb: &str, e: &DecisionEvent) -> i32 {
    match append_line(&decisions_path(), e) {
        Ok(()) => {
            println!("{}", serde_json::json!({ verb: e.id, "kind": e.kind }));
            0
        }
        Err(err) => {
            eprintln!("decision {verb}: {err}");
            1
        }
    }
}

fn list(args: &[String]) -> i32 {
    let include_archived = args.iter().any(|a| a == "--includeArchived");
    let views = read_decisions(include_archived);
    println!("{}", serde_json::to_string(&views).unwrap_or_else(|_| "[]".to_string()));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp log path with RAII cleanup (never touches the real dir).
    struct TmpLog(PathBuf);
    impl TmpLog {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("kiosk-decisions-{}.jsonl", crate::default_tab_id())))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn open(id: &str, project: &str, title: &str, at: u64) -> DecisionEvent {
        DecisionEvent {
            id: id.into(),
            kind: DecisionKind::Open,
            at,
            project: Some(project.into()),
            title: Some(title.into()),
            reco: Some(format!("reco-{title}")),
            files: vec![format!("~/Dev/outbox/{id}.md")],
            ..Default::default()
        }
    }
    fn ev(id: &str, kind: DecisionKind, at: u64) -> DecisionEvent {
        DecisionEvent { id: id.into(), kind, at, ..Default::default() }
    }

    // PD1 ⭐ REAL-FS (anti built≠wired): write real events to a temp jsonl, fold them,
    // and assert the whole progression + sticky-archived rule + determinism.
    #[test]
    fn pd1_fold_progression_and_sticky_archive_on_real_fs() {
        let log = TmpLog::new();
        // A transverse mix: one harness decision progressing, one kalpin still open.
        append_line(log.path(), &open("ra1c", "harness", "Deploy RA1c", 100)).unwrap();
        append_line(log.path(), &open("kb-crc", "kalpin", "CRC tour", 101)).unwrap();

        // open → the decision shows, state=open, content present, project transverse.
        let v0 = read_decisions_at(log.path(), false);
        assert_eq!(v0.len(), 2, "both open decisions appear");
        let ra1c = v0.iter().find(|d| d.id == "ra1c").expect("ra1c present");
        assert_eq!(ra1c.state, DecisionState::Open);
        assert_eq!(ra1c.project.as_deref(), Some("harness"));
        assert_eq!(ra1c.title.as_deref(), Some("Deploy RA1c"));
        assert_eq!(ra1c.files, vec!["~/Dev/outbox/ra1c.md"]);
        assert!(v0.iter().any(|d| d.project.as_deref() == Some("kalpin")), "transverse: kalpin too");

        // read → tranched: the STATE transits.
        append_line(log.path(), &ev("ra1c", DecisionKind::Read, 110)).unwrap();
        assert_eq!(read_decisions_at(log.path(), false).iter().find(|d| d.id == "ra1c").unwrap().state, DecisionState::Read);
        let mut tr = ev("ra1c", DecisionKind::Tranched, 120);
        tr.verdict = Some("GO".into());
        append_line(log.path(), &tr).unwrap();
        let after_tr = read_decisions_at(log.path(), false);
        let ra1c = after_tr.iter().find(|d| d.id == "ra1c").unwrap();
        assert_eq!(ra1c.state, DecisionState::Tranched);
        assert_eq!(ra1c.verdict.as_deref(), Some("GO"), "the verdict rides the fold");

        // archived → HIDDEN from the default list, SURFACED (state:archived) with the flag.
        append_line(log.path(), &ev("ra1c", DecisionKind::Archived, 130)).unwrap();
        assert!(read_decisions_at(log.path(), false).iter().all(|d| d.id != "ra1c"), "archived hidden by default");
        let all = read_decisions_at(log.path(), true);
        assert_eq!(all.iter().find(|d| d.id == "ra1c").unwrap().state, DecisionState::Archived, "surfaced with --includeArchived");

        // ⭐ STICKY: a later read/tranched does NOT resurrect an archived decision…
        append_line(log.path(), &ev("ra1c", DecisionKind::Read, 140)).unwrap();
        append_line(log.path(), &ev("ra1c", DecisionKind::Tranched, 141)).unwrap();
        assert!(read_decisions_at(log.path(), false).iter().all(|d| d.id != "ra1c"), "read/tranched after archive do NOT un-archive (sticky)");

        // …only an explicit re-OPEN does (with fresh content = latest-open-wins).
        append_line(log.path(), &open("ra1c", "harness", "Deploy RA1c v2", 150)).unwrap();
        let reopened = read_decisions_at(log.path(), false);
        let ra1c = reopened.iter().find(|d| d.id == "ra1c").expect("re-opened → back in the default list");
        assert_eq!(ra1c.state, DecisionState::Open, "explicit re-open resurrects");
        assert_eq!(ra1c.title.as_deref(), Some("Deploy RA1c v2"), "content axis = latest open wins");

        // Determinism: two folds of the same log are byte-identical.
        let a = serde_json::to_string(&read_decisions_at(log.path(), true)).unwrap();
        let b = serde_json::to_string(&read_decisions_at(log.path(), true)).unwrap();
        assert_eq!(a, b, "the fold is deterministic");
    }

    // PD2 (Olympe #2792 verdict-staleness): a verdict belongs to the CYCLE it was ruled
    // in. After a re-open (state=Open), a pre-archive Tranched verdict MUST NOT ride the
    // view — an Open decision showing a verdict is semantically bancal.
    #[test]
    fn pd2_reopen_after_tranch_drops_the_stale_verdict() {
        let log = TmpLog::new();
        append_line(log.path(), &open("d", "harness", "v1", 1)).unwrap();
        let mut tr = ev("d", DecisionKind::Tranched, 2);
        tr.verdict = Some("GO".into());
        append_line(log.path(), &tr).unwrap();
        // Same cycle: state=Tranched, the verdict rides the view.
        let ruled = read_decisions_at(log.path(), false);
        let d = ruled.iter().find(|x| x.id == "d").unwrap();
        assert_eq!(d.state, DecisionState::Tranched);
        assert_eq!(d.verdict.as_deref(), Some("GO"), "verdict shows within its cycle");

        // Archive then re-open → a NEW cycle. The old GO is stale.
        append_line(log.path(), &ev("d", DecisionKind::Archived, 3)).unwrap();
        append_line(log.path(), &open("d", "harness", "v2", 4)).unwrap();
        let reopened = read_decisions_at(log.path(), false);
        let d = reopened.iter().find(|x| x.id == "d").expect("re-opened → visible");
        assert_eq!(d.state, DecisionState::Open, "re-open resets the state");
        assert_eq!(d.verdict, None, "a pre-archive verdict does NOT ride a re-opened decision");

        // Ruling the NEW cycle surfaces its OWN verdict (the mechanism still works).
        let mut tr2 = ev("d", DecisionKind::Tranched, 5);
        tr2.verdict = Some("NO-GO".into());
        append_line(log.path(), &tr2).unwrap();
        assert_eq!(
            read_decisions_at(log.path(), false).iter().find(|x| x.id == "d").unwrap().verdict.as_deref(),
            Some("NO-GO"),
            "the new cycle's verdict surfaces"
        );
    }

    // PD1: the CONTENT axis = latest open|update wins (append order), independent of the
    // STATE axis — an `update` revises the content without changing state.
    #[test]
    fn pd1_content_axis_latest_open_or_update_wins() {
        let log = TmpLog::new();
        append_line(log.path(), &open("d", "harness", "v1", 1)).unwrap();
        append_line(log.path(), &ev("d", DecisionKind::Read, 2)).unwrap();
        // an update revises content but leaves state=read.
        let mut up = ev("d", DecisionKind::Update, 3);
        up.title = Some("v2".into());
        up.reco = Some("updated reco".into());
        append_line(log.path(), &up).unwrap();
        let v = read_decisions_at(log.path(), false);
        let d = v.iter().find(|x| x.id == "d").unwrap();
        assert_eq!(d.title.as_deref(), Some("v2"), "latest content (update) wins");
        assert_eq!(d.reco.as_deref(), Some("updated reco"));
        assert_eq!(d.state, DecisionState::Read, "update does NOT touch the state axis");
    }
}
