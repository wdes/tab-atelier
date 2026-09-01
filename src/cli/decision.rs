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
    /// `PD3b`: the EXPLICIT (original → archived) move pairs recorded at archive time.
    /// `reopen_restore` reads THIS mapping to move each file back — never re-derives by
    /// basename (which collides across decisions sharing a filename). `archived`-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<ArchivedMove>,
}

/// ``PD3b``: one archived file's EXPLICIT round-trip mapping — where it came from and where
/// it now lives. Stored on the `archived` event so a re-open restores it exactly, with
/// zero basename guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedMove {
    pub original: String,
    pub archived: String,
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
    let state = state_of(events);
    // PD3: an archived decision's bundle has physically MOVED to `_archive/AAAA-MM/` —
    // present the archived paths (recorded on the `archived` event) so the panel links
    // stay valid; otherwise the content's (live) outbox paths.
    let files = if state == DecisionState::Archived {
        events
            .iter()
            .rev()
            .find(|e| e.kind == DecisionKind::Archived)
            .map(|e| e.files.clone())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| content.files.clone())
    } else {
        content.files.clone()
    };
    Some(DecisionView {
        id: id.to_string(),
        project: content.project.clone(),
        title: content.title.clone(),
        why_gated: content.why_gated.clone(),
        reco: content.reco.clone(),
        effort: content.effort.clone(),
        files,
        state,
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
// PD3 — archiving: at `tranched → archived`, MOVE the decision's bundle files to
// `<outbox>/_archive/AAAA-MM/` (monthly), reversibly. A re-open moves them back. NEVER
// deletes — the anti-entassement discipline keeps the live outbox to the `vif` only,
// without ever losing a byte. All fns are path-injected (outbox + log) so they're
// testable against temp dirs (`std::env::set_var` is denied crate-wide).
// ---------------------------------------------------------------------------

/// The outbox base holding decision bundles. Honors `TAB_ATELIER_OUTBOX_PATH` (a
/// test/ops full-path override), else `~/Dev/outbox`.
#[must_use]
pub fn outbox_base() -> PathBuf {
    if let Ok(p) = std::env::var("TAB_ATELIER_OUTBOX_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Dev").join("outbox")
}

/// Expand a leading `~/` to `$HOME` so a stored path resolves to a real file on disk.
fn resolve(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// `AAAA-MM` (UTC) for a unix-seconds timestamp — the monthly archive bucket.
#[must_use]
fn archive_month(unix_secs: u64) -> String {
    let secs = i64::try_from(unix_secs).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp(secs, 0)
        .map_or_else(|| "unknown".to_string(), |dt| dt.format("%Y-%m").to_string())
}

/// Move a file, falling back to copy+remove when `rename` fails across devices. The
/// original always ends up gone and its bytes present at `dst` (a move, not a delete).
///
/// # Errors
/// Propagates the copy / remove I/O error when a cross-device rename fallback also fails.
fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst)?;
    std::fs::remove_file(src)
}

/// Move each existing file of `files` into `<outbox>/_archive/AAAA-MM/<id>/`.
///
/// ``PD3b``: the per-decision `<id>` sub-dir NAMESPACES the archive, so two decisions that
/// archive the same basename (`plan.md`) in the same month no longer clobber each other
/// on disk. Returns the `(original, archived)` pairs actually moved. `mkdir -p` the id
/// dir; a missing source is skipped (idempotent). NEVER deletes.
///
/// # Errors
/// Propagates a create-dir / move I/O error.
pub fn archive_files(outbox: &Path, id: &str, files: &[String], unix_secs: u64) -> std::io::Result<Vec<(String, String)>> {
    let dir = outbox.join("_archive").join(archive_month(unix_secs)).join(id);
    let mut moved = Vec::new();
    for f in files {
        let src = resolve(f);
        if !src.exists() {
            continue;
        }
        let Some(name) = src.file_name() else { continue };
        std::fs::create_dir_all(&dir)?;
        let dst = dir.join(name);
        move_file(&src, &dst)?;
        moved.push((f.clone(), dst.to_string_lossy().into_owned()));
    }
    Ok(moved)
}

/// Move a decision's archived files BACK to their originals (reversibility), from the
/// EXPLICIT stored mapping (``PD3b``) — never a basename re-derivation.
///
/// For each `ArchivedMove`, `mkdir -p` the original's parent and move the archived file
/// back. A missing archived file is skipped. NEVER deletes.
///
/// 🟡(b) guard: if the original was RE-CREATED between archive and restore, we do NOT
/// overwrite it with the older archived copy (that would lose the newer bytes) — we
/// leave the archived copy in place and skip. Reversibility must never destroy a newer
/// original. (ponytail: the two live side by side; a future merge/compare is out of
/// scope — the invariant that matters, no byte lost, holds.)
///
/// # Errors
/// Propagates a create-dir / move I/O error.
pub fn restore_files(pairs: &[ArchivedMove]) -> std::io::Result<()> {
    for ArchivedMove { original, archived } in pairs {
        let src = resolve(archived);
        if !src.exists() {
            continue;
        }
        let dst = resolve(original);
        if dst.exists() {
            // 🟡(b): a newer original exists — don't clobber it with the archived copy.
            eprintln!("decision restore: original '{original}' already exists — archived copy kept, not overwritten");
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        move_file(&src, &dst)?;
    }
    Ok(())
}

/// The EXPLICIT (original → archived) mapping stored on the LATEST `archived` event
/// (``PD3b``) — what `reopen_restore` moves back, with zero basename guessing.
fn archived_moves(events: &[DecisionEvent]) -> Vec<ArchivedMove> {
    events.iter().rev().find(|e| e.kind == DecisionKind::Archived).map(|e| e.moves.clone()).unwrap_or_default()
}

/// PD3: archive a ruled decision — move its bundles then append the `archived` event.
///
/// Moves the content's bundle files to `<outbox>/_archive/AAAA-MM/` and appends the
/// `archived` event (recording the moved paths + the month dir). The decision then
/// leaves the active list; its panel links point at the archive. Appended even when
/// nothing moved (empty / already-archived files) so the state transition is reliable.
/// Returns the archive dir. Call right after a `tranched`.
///
/// # Errors
/// Propagates a move / append I/O error.
pub fn archive_decision(log: &Path, outbox: &Path, id: &str, unix_secs: u64) -> std::io::Result<String> {
    let body = std::fs::read_to_string(log).unwrap_or_default();
    let events: Vec<DecisionEvent> = parse_decisions(&body).into_iter().filter(|e| e.id == id).collect();
    let files = events.iter().rfind(|e| e.kind.is_content()).map(|e| e.files.clone()).unwrap_or_default();
    // PD3b: namespace the move by `id`. ponytail 🟡(a): move-then-append is NOT
    // crash-atomic — a crash between the moves and the append leaves the files in the
    // id dir but the log un-updated. NO byte is lost (nothing is deleted; the files are
    // in `_archive/<month>/<id>/`), so it's recoverable, not data-loss; the upgrade path
    // is an intent record (append a `moving` marker first) or a reconciliation sweep.
    let moved = archive_files(outbox, id, &files, unix_secs)?;
    let dir = outbox.join("_archive").join(archive_month(unix_secs)).join(id).to_string_lossy().into_owned();
    let archived = DecisionEvent {
        id: id.to_string(),
        kind: DecisionKind::Archived,
        at: unix_secs,
        // Panel links point at the archived copies…
        files: moved.iter().map(|(_, new)| new.clone()).collect(),
        archived_path: Some(dir.clone()),
        // …and the EXPLICIT mapping restores them exactly (PD3b — no basename guessing).
        moves: moved.into_iter().map(|(original, archived)| ArchivedMove { original, archived }).collect(),
        ..Default::default()
    };
    append_line(log, &archived)?;
    Ok(dir)
}

/// PD3 reversibility: bring an archived decision's bundle back before a re-open.
///
/// If `id` is currently archived in `log`, move its archived files back to the original
/// outbox locations. Best-effort; call right before appending a re-`open`. A non-archived
/// id is a no-op.
///
/// # Errors
/// Propagates a move I/O error while restoring.
pub fn reopen_restore(log: &Path, id: &str) -> std::io::Result<()> {
    let body = std::fs::read_to_string(log).unwrap_or_default();
    let events: Vec<DecisionEvent> = parse_decisions(&body).into_iter().filter(|e| e.id == id).collect();
    if state_of(&events) != DecisionState::Archived {
        return Ok(());
    }
    // PD3b: restore from the EXPLICIT stored mapping, never a basename re-derivation.
    restore_files(&archived_moves(&events))
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

/// PD4 idempotence predicate: is this incoming `open` a NO-OP re-run?
///
/// True iff the decision is ALREADY `open` AND the latest content equals the incoming
/// (same project/title/why/reco/effort/files). A re-run of the same digest is then a
/// no-op — the log doesn't grow. A re-open of a `read`/`tranched`/`archived` decision is
/// NOT a no-op (it's a real state change), and a content change always appends. PURE.
#[must_use]
pub fn is_noop_open(events: &[DecisionEvent], incoming: &DecisionEvent) -> bool {
    if state_of(events) != DecisionState::Open {
        return false;
    }
    events.iter().rfind(|e| e.kind.is_content()).is_some_and(|c| {
        c.project == incoming.project
            && c.title == incoming.title
            && c.why_gated == incoming.why_gated
            && c.reco == incoming.reco
            && c.effort == incoming.effort
            && c.files == incoming.files
    })
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
    // PD4 IDEMPOTENCE (the DRY dedup every producer inherits): a re-run of the same open
    // on an already-open decision is a NO-OP — don't grow the log / duplicate.
    let path = decisions_path();
    let existing: Vec<DecisionEvent> =
        parse_decisions(&std::fs::read_to_string(&path).unwrap_or_default()).into_iter().filter(|ev| ev.id == e.id).collect();
    if is_noop_open(&existing, &e) {
        println!("{}", serde_json::json!({ "push": e.id, "noop": true }));
        return 0;
    }
    // PD3 reversibility: re-opening an ARCHIVED decision brings its bundle back from the
    // archive before the `open` lands (best-effort; nothing is ever destroyed).
    if let Err(err) = reopen_restore(&path, &e.id) {
        eprintln!("decision push: restore of the archived bundle failed — {err}");
    }
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
    let rc = append_or_report(if kind == DecisionKind::Read { "read" } else { "tranch" }, &e);
    // PD3: a `tranch` triggers archiving — file the bundle under _archive/AAAA-MM/ and
    // append the `archived` event (same as the HTTP route), so the CLI and UI agree.
    if rc == 0
        && kind == DecisionKind::Tranched
        && let Err(err) = archive_decision(&decisions_path(), &outbox_base(), &e.id, e.at)
    {
        eprintln!("decision tranch: archive failed — {err}");
    }
    rc
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

    /// A unique temp outbox dir with RAII cleanup (never touches the real ~/Dev/outbox).
    struct TmpOutbox(PathBuf);
    impl TmpOutbox {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("kiosk-outbox-{}", crate::default_tab_id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpOutbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // PD3 ⭐ REAL-FS (anti built≠wired, NO mock): a REAL bundle file on disk is physically
    // MOVED to `_archive/AAAA-MM/` at tranch→archive, the read-model points at the archive,
    // and a re-open MOVES IT BACK (reversibility) — all proven on the actual filesystem.
    #[test]
    fn pd3_archive_moves_bundle_on_disk_and_reopen_restores() {
        let log = TmpLog::new();
        let outbox = TmpOutbox::new();
        let bundle = outbox.path().join("ra1c-deploy.md");
        std::fs::write(&bundle, b"the RA1c deploy bundle").unwrap();
        let bundle_str = bundle.to_string_lossy().into_owned();

        // push an open decision that references the REAL bundle, then rule it.
        let mut o = open("ra1c", "harness", "Deploy RA1c", 100);
        o.files = vec![bundle_str];
        append_line(log.path(), &o).unwrap();
        let mut tr = ev("ra1c", DecisionKind::Tranched, 110);
        tr.verdict = Some("GO".into());
        append_line(log.path(), &tr).unwrap();

        // Archive (the tranch route's step): move the bundle to _archive/AAAA-MM/.
        let ts = 1_756_700_000; // fixed timestamp -> deterministic AAAA-MM (2025-08)
        let dir = archive_decision(log.path(), outbox.path(), "ra1c", ts).unwrap();

        // The file REALLY moved: the original is gone, the archive copy exists (bytes kept).
        assert!(!bundle.exists(), "the original bundle left the live outbox");
        let month = archive_month(ts);
        // PD3b: namespaced by the decision id → `_archive/<month>/<id>/<basename>`.
        let archived = outbox.path().join("_archive").join(&month).join("ra1c").join("ra1c-deploy.md");
        assert!(archived.exists(), "the bundle now lives in _archive/{month}/ra1c");
        assert_eq!(std::fs::read_to_string(&archived).unwrap(), "the RA1c deploy bundle", "bytes preserved by the move");

        // The read-model: archived, hidden by default; the view's files point at the
        // archive (links stay valid) and the verdict rides the archived view (same cycle).
        assert!(read_decisions_at(log.path(), false).iter().all(|d| d.id != "ra1c"), "archived hidden by default");
        let all = read_decisions_at(log.path(), true);
        let d = all.iter().find(|d| d.id == "ra1c").unwrap();
        assert_eq!(d.state, DecisionState::Archived);
        assert_eq!(d.files, vec![archived.to_string_lossy().into_owned()], "view.files points at the archive path");
        assert_eq!(d.verdict.as_deref(), Some("GO"), "the verdict rides the archived view");
        // The archived EVENT records archived_path = the month dir.
        let body = std::fs::read_to_string(log.path()).unwrap();
        let arch_ev = parse_decisions(&body).into_iter().rev().find(|e| e.id == "ra1c" && e.kind == DecisionKind::Archived).unwrap();
        assert_eq!(arch_ev.archived_path.as_deref(), Some(dir.as_str()), "archived_path = the month dir");

        // ⭐ REVERSIBILITY: a re-open MOVES the bundle back to its original path (nothing
        // is ever destroyed; a moved bundle can come home).
        reopen_restore(log.path(), "ra1c").unwrap();
        assert!(bundle.exists(), "re-open restored the bundle to its original outbox path");
        assert!(!archived.exists(), "the archive copy moved back (no duplication)");
        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), "the RA1c deploy bundle", "bytes intact after the round-trip");
        // The explicit re-open event (as `push` appends after restore) -> state=open again.
        append_line(log.path(), &open("ra1c", "harness", "Deploy RA1c v2", 120)).unwrap();
        assert_eq!(read_decisions_at(log.path(), false).iter().find(|d| d.id == "ra1c").unwrap().state, DecisionState::Open);
    }

    // PD3b ⭐ ANTI-CLOBBER (the Olympe 🟡 bug, killed): TWO distinct decisions (A, B)
    // each archive a file NAMED `plan.md` in the SAME month. Before PD3b they'd both
    // land at `_archive/<month>/plan.md` → the 2nd move clobbered the 1st (DATA LOSS)
    // and restore returned the WRONG bytes. Now the per-id sub-dir isolates them AND
    // restore reads the EXPLICIT mapping — each decision gets back ITS OWN bytes.
    #[test]
    fn pd3b_two_decisions_same_basename_same_month_no_clobber() {
        let log = TmpLog::new();
        let outbox = TmpOutbox::new();
        let ts = 1_756_700_000; // same month for BOTH → the collision window
        let month = archive_month(ts);

        // Two REAL, DISTINCT `plan.md` files with DIFFERENT bytes, one per decision.
        let sub_a = outbox.path().join("proj-a");
        let sub_b = outbox.path().join("proj-b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        let plan_a = sub_a.join("plan.md");
        let plan_b = sub_b.join("plan.md");
        std::fs::write(&plan_a, b"PLAN OF DECISION A").unwrap();
        std::fs::write(&plan_b, b"PLAN OF DECISION B").unwrap();

        for (id, plan) in [("dec-a", &plan_a), ("dec-b", &plan_b)] {
            let mut o = open(id, "harness", id, 100);
            o.files = vec![plan.to_string_lossy().into_owned()];
            append_line(log.path(), &o).unwrap();
            append_line(log.path(), &ev(id, DecisionKind::Tranched, 105)).unwrap();
            archive_decision(log.path(), outbox.path(), id, ts).unwrap();
        }

        // NO CLOBBER: both archives coexist, each under its own id namespace, each SES octets.
        let arch_a = outbox.path().join("_archive").join(&month).join("dec-a").join("plan.md");
        let arch_b = outbox.path().join("_archive").join(&month).join("dec-b").join("plan.md");
        assert!(arch_a.exists() && arch_b.exists(), "both archived plan.md coexist (namespaced by id)");
        assert_eq!(std::fs::read_to_string(&arch_a).unwrap(), "PLAN OF DECISION A", "A's archive holds A's bytes");
        assert_eq!(std::fs::read_to_string(&arch_b).unwrap(), "PLAN OF DECISION B", "B's archive holds B's bytes");

        // RESTORE via the EXPLICIT mapping: each decision gets back ITS OWN bytes.
        reopen_restore(log.path(), "dec-a").unwrap();
        reopen_restore(log.path(), "dec-b").unwrap();
        assert_eq!(std::fs::read_to_string(&plan_a).unwrap(), "PLAN OF DECISION A", "restore(A) → A's bytes at A's original");
        assert_eq!(std::fs::read_to_string(&plan_b).unwrap(), "PLAN OF DECISION B", "restore(B) → B's bytes at B's original");

        // The archived events carry the EXPLICIT (original → archived) mapping (not a
        // basename re-derivation) — the mechanism that makes the above robust.
        let events = parse_decisions(&std::fs::read_to_string(log.path()).unwrap());
        let a_moves = archived_moves(&events.iter().filter(|e| e.id == "dec-a").cloned().collect::<Vec<_>>());
        assert_eq!(a_moves.len(), 1);
        assert_eq!(a_moves[0].original, plan_a.to_string_lossy());
        assert!(a_moves[0].archived.ends_with("dec-a/plan.md"), "A's mapping points at A's namespaced archive");
    }

    // PD3b 🟡(b) guard: a restore does NOT overwrite an original that was RE-CREATED
    // between archive and restore (that newer file's bytes must survive).
    #[test]
    fn pd3b_restore_never_clobbers_a_recreated_original() {
        let outbox = TmpOutbox::new();
        let arch_dir = outbox.path().join("_archive").join("2025-08").join("d");
        std::fs::create_dir_all(&arch_dir).unwrap();
        let archived = arch_dir.join("plan.md");
        std::fs::write(&archived, b"OLD archived bytes").unwrap();
        let original = outbox.path().join("plan.md");
        std::fs::write(&original, b"NEW recreated bytes").unwrap(); // recreated after archive

        restore_files(&[ArchivedMove {
            original: original.to_string_lossy().into_owned(),
            archived: archived.to_string_lossy().into_owned(),
        }])
        .unwrap();
        // The NEWER original is untouched; the archived copy stays put (nothing lost).
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "NEW recreated bytes", "recreated original preserved");
        assert!(archived.exists(), "the archived copy is kept, not force-moved over the newer file");
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

    // PD4: `is_noop_open` dedups ONLY an identical re-run on an already-open decision —
    // the DRY idempotence every producer inherits.
    #[test]
    fn pd4_is_noop_open_dedups_an_identical_rerun_only() {
        let o1 = open("d", "harness", "T", 1);
        let same = open("d", "harness", "T", 2); // same content, different `at`
        assert!(is_noop_open(std::slice::from_ref(&o1), &same), "identical re-run on an open decision → no-op (idempotent)");

        // A content change → NOT a no-op (it must append the revision).
        let changed = open("d", "harness", "T2", 2);
        assert!(!is_noop_open(std::slice::from_ref(&o1), &changed), "changed content → appends");

        // A re-open of a read/tranched/archived decision is a REAL state change.
        assert!(!is_noop_open(&[o1.clone(), ev("d", DecisionKind::Read, 3)], &same), "re-open of a read decision is meaningful");
        assert!(
            !is_noop_open(&[o1, ev("d", DecisionKind::Archived, 3)], &same),
            "re-open of an archived decision resurrects (not a no-op)"
        );
        // The first push (no prior content) is never a no-op.
        assert!(!is_noop_open(&[], &same), "first push is never a no-op");
    }
}
