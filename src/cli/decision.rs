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
    /// A 2-3 line résumé rendered UNDER the bold title, above the toggle (distinct from
    /// `title` — NOT a rename). `compose` fills it; `push --summary` sets it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_gated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reco: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Long-form body (the résumé court is `title`; `detail` is the expandable
    /// description the KIOSK toggle reveals). Optional — no `detail` → no toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// FU1 (ping-on-tranch): WHO pushed the decision (`decision push --from <x>`).
    /// Rides the content event so a `tranch` can actively notify the pusher. Optional
    /// (rétro-compat: an old event without it → no pusher, tichef still notified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
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
    /// The 2-3 line résumé shown under the title (NEW; distinct from `title`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_gated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reco: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The long-form body the KIOSK toggle reveals (absent → no toggle).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
        summary: content.summary.clone(),
        why_gated: content.why_gated.clone(),
        reco: content.reco.clone(),
        effort: content.effort.clone(),
        detail: content.detail.clone(),
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
// FU1 — ping-on-tranch: on a `tranch`, ACTIVELY notify the pusher + tichef, in
// ADDITION to the passive `decision list` log (which stays intact). Dual channel,
// modelled on the daemon `RESTART_DONE` wake: (a) a durable NOTE on the `decisions`
// topic (pull-friendly), (b) a targeted, regulated swamp PING to pusher+tichef
// (push). Anti-herd: at most those two targets — NEVER a fan-out. 1st RA2 brick.
// ---------------------------------------------------------------------------

/// tichef, the router — ALWAYS notified on a tranch (it routes the exec onward).
const TICHEF: &str = "tichef";

/// The pusher of a decision: the `from` on its latest content event (whoever ran
/// `decision push --from <x>`), if any. `None` → only tichef gets the ping.
#[must_use]
fn pusher_of(events: &[DecisionEvent]) -> Option<String> {
    events.iter().rfind(|e| e.kind.is_content()).and_then(|e| e.from.clone())
}

/// The tranch-notify payload: `DECISION_TRANCHED id=<id> verdict=<v> from=<pousseur> by=PO`.
/// `from=-` when the pusher is unknown (old event / no `--from`).
#[must_use]
pub fn tranch_notify_msg(id: &str, verdict: &str, pusher: Option<&str>) -> String {
    let from = pusher.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("-");
    format!("DECISION_TRANCHED id={id} verdict={verdict} from={from} by=PO")
}

/// The targets a tranch actively pings: the pusher (when known) + tichef,
/// de-duplicated (a pusher that IS tichef is pinged once). Anti-herd: at most two,
/// NEVER a fan-out. PURE.
#[must_use]
pub fn tranch_targets(pusher: Option<&str>) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(p) = pusher.map(str::trim).filter(|s| !s.is_empty()) {
        targets.push(p.to_string());
    }
    if !targets.iter().any(|t| t == TICHEF) {
        targets.push(TICHEF.to_string());
    }
    targets
}

/// What the tranch-notify did (FU1) — for the CLI + the tests to assert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranchNotifyReport {
    /// The durable `decisions`-topic note was attempted (pull fallback).
    pub note_posted: bool,
    /// The targets the ping actually reached (pusher and/or tichef).
    pub pinged: Vec<String>,
    /// Targets whose ping failed (best-effort; the note is the fallback).
    pub failed: usize,
}

/// The `tranch → notify` STEP (FU1), PATH + SEAM-INJECTED so the wiring is real-fs
/// testable (anti built≠wired). Reads the pusher from `log` (whoever `push --from`
/// recorded), then emits the dual-channel notify via the given `note`/`ping` seams.
/// `mark` calls this with the live log + the real seams; a test calls it with a temp
/// log + temp-fs seams to prove the whole tranch-path fires end to end.
fn notify_on_tranch<N, P>(log: &Path, id: &str, verdict: &str, note: N, ping: P) -> TranchNotifyReport
where
    N: FnOnce(&str, &str, &str),
    P: FnMut(&str, &str) -> std::io::Result<()>,
{
    let body = std::fs::read_to_string(log).unwrap_or_default();
    let events: Vec<DecisionEvent> = parse_decisions(&body).into_iter().filter(|ev| ev.id == id).collect();
    let pusher = pusher_of(&events);
    emit_tranch_notify(id, verdict, pusher.as_deref(), note, ping)
}

/// Emit the active tranch-notify, PURE + best-effort + mockable (FU1).
///
/// (a) one durable `note` on the `decisions` topic (the pull-friendly log, caught
/// by any poller), then (b) a targeted `ping` to pusher+tichef (the push — the
/// wake that closes the loop). Anti-herd: at most pusher+tichef, never a fan-out.
/// Both effects are INJECTED seams so the core is unit-tested without a live daemon;
/// a failing ping is COUNTED, never propagated (the note is the fallback).
pub fn emit_tranch_notify<N, P>(id: &str, verdict: &str, pusher: Option<&str>, note: N, mut ping: P) -> TranchNotifyReport
where
    N: FnOnce(&str, &str, &str),
    P: FnMut(&str, &str) -> std::io::Result<()>,
{
    let msg = tranch_notify_msg(id, verdict, pusher);
    // (a) durable pull fallback: one note on the `decisions` topic.
    note("decisions", "PO", &msg);
    // (b) low-latency push: a targeted ping per target (pusher + tichef).
    let (mut pinged, mut failed) = (Vec::new(), 0usize);
    for t in tranch_targets(pusher) {
        match ping(&t, &msg) {
            Ok(()) => pinged.push(t),
            Err(_) => failed += 1,
        }
    }
    TranchNotifyReport { note_posted: true, pinged, failed }
}

/// Resolve a target (exact name → uuid → case-insensitive name substring) to a
/// tab-id from a `/tabs` snapshot. Mirrors `delegate::resolve_target`'s name logic
/// so the tranch-ping addresses the same way `dispatch --to` does. PURE.
#[must_use]
fn resolve_tab_id(tabs: &[serde_json::Value], key: &str) -> Option<String> {
    let name_of = |t: &serde_json::Value| t.get("name").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let id_of = |t: &serde_json::Value| t.get("id").and_then(serde_json::Value::as_str).map(str::to_owned);
    if let Some(t) = tabs.iter().find(|t| name_of(t) == key) {
        return id_of(t);
    }
    if let Some(t) = tabs.iter().find(|t| id_of(t).as_deref() == Some(key)) {
        return id_of(t);
    }
    let lk = key.to_lowercase();
    let mut hits = tabs.iter().filter(|t| name_of(t).to_lowercase().contains(&lk));
    match (hits.next(), hits.next()) {
        (Some(t), None) => id_of(t),
        _ => None, // 0 or >1 matches → don't guess (best-effort; the note is the fallback)
    }
}

/// The REAL tranch-ping seam: resolve `target` (name|uuid) to a tab-id, then enqueue
/// a regulated swamp wake (submit=true → triggers the target's turn), deduped per
/// (decision, tranch-ts, target) so aligator can't double-deliver the SAME entry
/// while a re-tranch (fresh ts) always re-emits. Best-effort: an undiscoverable
/// daemon / unresolvable target errors so the caller counts it — the durable
/// `decisions` note remains the pull fallback.
///
/// # Errors
/// Propagates endpoint-discovery / tabs-fetch / resolution / swamp-append failures.
fn real_tranch_ping(id: &str, target: &str, msg: &str, at_secs: u64) -> std::io::Result<()> {
    let ep = crate::cli::share_link::discover_endpoint().map_err(std::io::Error::other)?;
    let tabs = crate::cli::share_link::fetch_tabs(&ep).map_err(std::io::Error::other)?;
    let uuid = resolve_tab_id(&tabs, target).ok_or_else(|| std::io::Error::other(format!("no tab matches {target:?}")))?;
    crate::cli::restart_wake::push_swamp_input(
        &crate::cli::aligator::swamp_path(),
        &uuid,
        msg,
        true, // ⭐ trigger the target's turn (the active notify), not just a marker.
        at_secs.saturating_mul(1000),
        crate::cli::aligator::Priority::Status,
        Some(format!("decision-tranched-{id}-{at_secs}-{uuid}")),
    )
}

// ---------------------------------------------------------------------------
// CLI — `tab-atelier decision push|read|tranch|list [--includeArchived]`.
// ---------------------------------------------------------------------------

fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// Collect EVERY value following each occurrence of `flag` — a REPEATABLE option
/// (`--options` / `--command` / `--files` in `compose`). Order-preserving. PURE.
fn args_after_all<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == flag)
        .filter_map(|(i, _)| args.get(i + 1))
        .map(String::as_str)
        .collect()
}

/// `tab-atelier decision <push|read|tranch|list>` (PD1). Append-only mutations +
/// the folded read-model. No UI.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("push") => push(&args[1..]),
        Some("compose") => compose(&args[1..]),
        Some("read") => mark(&args[1..], DecisionKind::Read),
        Some("tranch") => mark(&args[1..], DecisionKind::Tranched),
        Some("list") => list(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 tab-atelier decision push --id <id> --project <p> --title <t> [--summary <s>] [--why <w>] [--reco <r>] [--effort <e>] [--detail <body>] [--files a,b] [--from <pusher>]\n  \
                 tab-atelier decision compose --id <id> --title <t> [--summary <s>] [--from <x>] [--enjeux <e>] [--options \"A: …\"]… [--reco <r>] [--effort <e>] [--files <f>]… [--command <c>]… [--link <url>] [--reopen]\n  \
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
            && c.summary == incoming.summary
            && c.why_gated == incoming.why_gated
            && c.reco == incoming.reco
            && c.effort == incoming.effort
            && c.detail == incoming.detail
            && c.files == incoming.files
            && c.from == incoming.from
    })
}

/// What a `push` did (`PD4b`) — for the CLI to report + for tests to assert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The `open` was appended (new decision, or an open decision's content changed).
    Appended,
    /// No-op: an identical re-run on an already-open decision (PD4 idempotence).
    NoopIdentical,
    /// No-op: the decision is SETTLED (`tranched`/`archived`) — the routine producer
    /// must NOT resurrect it. Only an explicit re-open does (`PD4b`). ⭐ the fix.
    NoopSettled,
    /// Explicit re-open: the settled/archived decision was resurrected (bundle restored
    /// from the archive, then the `open` appended). The ONLY resurrection path.
    Reopened,
}

/// `PD4b` — the routine-vs-explicit push core, PATH-INJECTED (testable real-fs).
///
/// `reopen == false` (the ROUTINE producer): NO-OP if the decision is SETTLED
/// (`tranched`/`archived`) — a timer re-emitting `open` must never fight the PO's ruling
/// / the PD3 sticky-archive — OR if it's an identical re-run of an already-open decision
/// (PD4 idempotence). Otherwise append. `reopen == true` (EXPLICIT): bypass the no-ops,
/// restore the archived bundle (`reopen_restore`), then append — the SOLE resurrection.
///
/// # Errors
/// Propagates the restore / append I/O error.
pub fn push_event(log: &Path, e: &DecisionEvent, reopen: bool) -> std::io::Result<PushOutcome> {
    let existing: Vec<DecisionEvent> =
        parse_decisions(&std::fs::read_to_string(log).unwrap_or_default()).into_iter().filter(|ev| ev.id == e.id).collect();
    if reopen {
        // PD3 reversibility, now the EXPLICIT path only: bring the bundle home first.
        reopen_restore(log, &e.id)?;
        append_line(log, e)?;
        return Ok(PushOutcome::Reopened);
    }
    // ⭐ PD4b: never let the routine producer resurrect a SETTLED decision.
    if matches!(state_of(&existing), DecisionState::Tranched | DecisionState::Archived) {
        return Ok(PushOutcome::NoopSettled);
    }
    // PD4 idempotence: an identical re-run on an already-open decision.
    if is_noop_open(&existing, e) {
        return Ok(PushOutcome::NoopIdentical);
    }
    append_line(log, e)?;
    Ok(PushOutcome::Appended)
}

fn push(args: &[String]) -> i32 {
    let Some(id) = arg_after(args, "--id").filter(|s| !s.trim().is_empty()) else {
        eprintln!("decision push: --id is required");
        return 2;
    };
    // PD4b: the ROUTINE producer path is the default; `--reopen` is the EXPLICIT, sole
    // resurrection path for a settled/archived decision.
    let reopen = args.iter().any(|a| a == "--reopen");
    let files = arg_after(args, "--files")
        .map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let e = DecisionEvent {
        id: id.to_string(),
        kind: DecisionKind::Open,
        at: crate::unix_millis() / 1000,
        project: arg_after(args, "--project").map(str::to_string),
        title: arg_after(args, "--title").map(str::to_string),
        summary: arg_after(args, "--summary").map(str::to_string),
        why_gated: arg_after(args, "--why").map(str::to_string),
        reco: arg_after(args, "--reco").map(str::to_string),
        effort: arg_after(args, "--effort").map(str::to_string),
        detail: arg_after(args, "--detail").map(str::to_string),
        files,
        // FU1: record the pusher so a later `tranch` can actively notify them.
        from: arg_after(args, "--from").map(str::to_string),
        ..Default::default()
    };
    match push_event(&decisions_path(), &e, reopen) {
        Ok(outcome) => {
            let noop = matches!(outcome, PushOutcome::NoopIdentical | PushOutcome::NoopSettled);
            let verb = if reopen { "reopen" } else { "push" };
            println!("{}", serde_json::json!({ verb: e.id, "outcome": format!("{outcome:?}"), "noop": noop }));
            0
        }
        Err(err) => {
            eprintln!("decision push: {err}");
            1
        }
    }
}

/// `decision compose` — the DÉTERMINISTE high-level authoring path (design in
/// `decision-compose-tool-design.md`). Named fields become a correctly-formatted `open`
/// event so nobody has to remember the mapping. `--title` is the bold heading, `--summary`
/// the NEW 2-3 line résumé under it, and the `detail` toggle body is assembled from
/// `--enjeux`, repeatable `--options`, `--reco`, `--effort`, repeatable `--command` (each
/// auto-fenced so the 📋 copy button appears), and `--link`. Repeatable `--files` become the
/// clickable file links; `--reopen` resurrects a settled decision. Delegates to the SAME
/// `push_event` primitive as `push` (zero divergence — `push` stays for scripts).
fn compose(args: &[String]) -> i32 {
    let Some(id) = arg_after(args, "--id").filter(|s| !s.trim().is_empty()) else {
        eprintln!("decision compose: --id is required");
        return 2;
    };
    let Some(title) = arg_after(args, "--title").filter(|s| !s.trim().is_empty()) else {
        eprintln!("decision compose: --title is required");
        return 2;
    };
    let reopen = args.iter().any(|a| a == "--reopen");
    let files = args_after_all(args, "--files").into_iter().map(str::to_string).collect();
    let detail = compose_detail(args);
    let e = DecisionEvent {
        id: id.to_string(),
        kind: DecisionKind::Open,
        at: crate::unix_millis() / 1000,
        project: arg_after(args, "--project").map(str::to_string),
        title: Some(title.to_string()),
        summary: arg_after(args, "--summary").map(str::to_string),
        detail: (!detail.is_empty()).then_some(detail),
        files,
        from: arg_after(args, "--from").map(str::to_string),
        ..Default::default()
    };
    match push_event(&decisions_path(), &e, reopen) {
        Ok(outcome) => {
            let noop = matches!(outcome, PushOutcome::NoopIdentical | PushOutcome::NoopSettled);
            let verb = if reopen { "reopen" } else { "compose" };
            println!("{}", serde_json::json!({ verb: e.id, "outcome": format!("{outcome:?}"), "noop": noop }));
            0
        }
        Err(err) => {
            eprintln!("decision compose: {err}");
            1
        }
    }
}

/// Assemble compose's structured flags into the `detail` body, in a FIXED order, as the
/// SIMPLE-markdown the KIOSK toggle renders: `**bold**` section headers + newlines, and a
/// fenced ```` ```sh ```` block per `--command` (so the 📋 copy button appears, 5f552e2).
/// Empty when no structured flag is present (→ no toggle, feature-detect). PURE.
fn compose_detail(args: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if let Some(enjeux) = arg_after(args, "--enjeux").filter(|s| !s.trim().is_empty()) {
        sections.push(format!("**Enjeux**\n{enjeux}"));
    }
    let options = args_after_all(args, "--options");
    if !options.is_empty() {
        let list = options.iter().map(|o| format!("- {o}")).collect::<Vec<_>>().join("\n");
        sections.push(format!("**Options**\n{list}"));
    }
    if let Some(reco) = arg_after(args, "--reco").filter(|s| !s.trim().is_empty()) {
        sections.push(format!("**Reco**\n{reco}"));
    }
    if let Some(effort) = arg_after(args, "--effort").filter(|s| !s.trim().is_empty()) {
        sections.push(format!("**Effort**\n{effort}"));
    }
    for cmd in args_after_all(args, "--command") {
        sections.push(format!("```sh\n{cmd}\n```"));
    }
    if let Some(link) = arg_after(args, "--link").filter(|s| !s.trim().is_empty()) {
        sections.push(format!("**Lien** — {link}"));
    }
    sections.join("\n\n")
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
    if rc == 0 && kind == DecisionKind::Tranched {
        // FU1: the verdict + state are now settled → ACTIVELY notify the pusher +
        // tichef (in addition to the passive `decision list` log). Best-effort: the
        // durable `decisions` note is the pull fallback if the swamp push can't reach.
        let report = notify_on_tranch(
            &decisions_path(),
            &e.id,
            e.verdict.as_deref().unwrap_or("-"),
            crate::cli::restart_wake::real_note,
            |target, msg| real_tranch_ping(&e.id, target, msg, e.at),
        );
        if report.failed > 0 {
            eprintln!("decision tranch: notify reached {:?}, {} target(s) unreachable (note posted as fallback)", report.pinged, report.failed);
        }
        // PD3: a `tranch` triggers archiving — file the bundle under _archive/AAAA-MM/
        // and append the `archived` event (same as the HTTP route), CLI ↔ UI agree.
        if let Err(err) = archive_decision(&decisions_path(), &outbox_base(), &e.id, e.at) {
            eprintln!("decision tranch: archive failed — {err}");
        }
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

        // ⭐ REVERSIBILITY via the EXPLICIT re-open path (PD4b): `push_event(.., reopen=true)`
        // restores the bundle AND appends the `open` — the SOLE resurrection path.
        push_event(log.path(), &open("ra1c", "harness", "Deploy RA1c v2", 120), true).unwrap();
        assert!(bundle.exists(), "explicit re-open restored the bundle to its original outbox path");
        assert!(!archived.exists(), "the archive copy moved back (no duplication)");
        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), "the RA1c deploy bundle", "bytes intact after the round-trip");
        let reopened = read_decisions_at(log.path(), false);
        let d = reopened.iter().find(|d| d.id == "ra1c").expect("explicit re-open → back in the active list");
        assert_eq!(d.state, DecisionState::Open, "explicit re-open resurrects to open");
        assert_eq!(d.title.as_deref(), Some("Deploy RA1c v2"), "the re-open's content lands");
    }

    // PD4b ⭐ ANTI-RESURRECTION (the Olympe 🟡 bug, killed): a SETTLED decision
    // (tranched → archived) re-pushed by the ROUTINE producer (push open, same id, no
    // --reopen) STAYS archived (out of the active list) and its bundle is NOT restored.
    // Only an EXPLICIT re-open resurrects.
    #[test]
    fn pd4b_routine_push_never_resurrects_a_settled_decision() {
        let log = TmpLog::new();
        let outbox = TmpOutbox::new();
        let bundle = outbox.path().join("dep.md");
        std::fs::write(&bundle, b"deploy bundle").unwrap();

        // open → tranch → archive (the PO ruled + the bundle filed).
        let mut o = open("dep", "harness", "Deploy", 100);
        o.files = vec![bundle.to_string_lossy().into_owned()];
        append_line(log.path(), &o).unwrap();
        append_line(log.path(), &ev("dep", DecisionKind::Tranched, 110)).unwrap();
        let ts = 1_756_700_000;
        archive_decision(log.path(), outbox.path(), "dep", ts).unwrap();
        assert!(!bundle.exists(), "bundle archived off the live outbox");
        assert!(read_decisions_at(log.path(), false).iter().all(|d| d.id != "dep"), "settled → hidden from the active list");

        // ⭐ the ROUTINE producer re-pushes the SAME open (a timer tick) → NO-OP settled.
        let outcome = push_event(log.path(), &o, false).unwrap();
        assert_eq!(outcome, PushOutcome::NoopSettled, "routine push on a settled decision is a no-op");
        // It STAYS archived — no resurrection — and the bundle is NOT back on disk.
        assert!(read_decisions_at(log.path(), false).iter().all(|d| d.id != "dep"), "still hidden after the routine re-push (not resurrected)");
        assert_eq!(read_decisions_at(log.path(), true).iter().find(|d| d.id == "dep").unwrap().state, DecisionState::Archived, "stays archived");
        assert!(!bundle.exists(), "the archived bundle was NOT resurrected on disk by the routine push");
        // The routine re-push appended NOTHING (log did not grow).
        let archived_events = parse_decisions(&std::fs::read_to_string(log.path()).unwrap()).into_iter().filter(|e| e.id == "dep").count();
        assert_eq!(archived_events, 3, "open + tranched + archived — the routine re-push added no event");

        // Only an EXPLICIT re-open resurrects (bundle back + state open).
        push_event(log.path(), &o, true).unwrap();
        assert!(bundle.exists(), "explicit re-open restores the bundle");
        assert_eq!(read_decisions_at(log.path(), false).iter().find(|d| d.id == "dep").unwrap().state, DecisionState::Open, "explicit re-open → open");
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

    // Kiosk detail-toggle ⭐ REAL-FS: the OPTIONAL `detail` (long-form body) rides the
    // content axis to the view — present when pushed, ABSENT (→ no toggle) otherwise
    // (rétro-compat), latest-content-wins, and part of the idempotence key.
    #[test]
    fn kiosk_detail_rides_the_fold_and_stays_optional() {
        let log = TmpLog::new();
        // A decision WITHOUT --detail → the view carries no detail (no toggle downstream).
        append_line(log.path(), &open("plain", "harness", "no detail", 1)).unwrap();
        // A decision WITH detail.
        let mut o = open("rich", "harness", "with detail", 2);
        o.detail = Some("Enjeux: …\nOptions: A / B\nReco: A".into());
        append_line(log.path(), &o).unwrap();

        let v = read_decisions_at(log.path(), false);
        assert_eq!(
            v.iter().find(|d| d.id == "plain").unwrap().detail,
            None,
            "no --detail → no detail on the view (rétro-compat, pas de toggle)"
        );
        let rich = v.iter().find(|d| d.id == "rich").unwrap();
        assert!(rich.detail.as_deref().unwrap().contains("Options: A / B"), "detail rides the fold verbatim");

        // The camelCase read-model serves `detail` (the key the panel feature-detects).
        let json = serde_json::to_string(rich).unwrap();
        assert!(json.contains("\"detail\":"), "the served read-model carries `detail`");
        // And a plain decision's JSON omits it (skip_serializing_if) → the panel sees no toggle.
        let plain_json = serde_json::to_string(v.iter().find(|d| d.id == "plain").unwrap()).unwrap();
        assert!(!plain_json.contains("\"detail\""), "a detail-less decision omits the key entirely");

        // Latest-content-wins on the detail axis (an `update` revises it).
        let mut up = ev("rich", DecisionKind::Update, 3);
        up.title = Some("with detail".into());
        up.detail = Some("v2 corps étendu".into());
        append_line(log.path(), &up).unwrap();
        assert_eq!(
            read_decisions_at(log.path(), false).iter().find(|d| d.id == "rich").unwrap().detail.as_deref(),
            Some("v2 corps étendu"),
            "latest content event's detail wins"
        );

        // Idempotence: same content but a NEW detail is NOT a no-op (it must append).
        let base = open("k", "harness", "T", 1);
        let mut with_detail = open("k", "harness", "T", 2);
        with_detail.detail = Some("body".into());
        assert!(!is_noop_open(std::slice::from_ref(&base), &with_detail), "adding a detail is a real content change");
        assert!(
            is_noop_open(std::slice::from_ref(&with_detail), &{
                let mut c = with_detail.clone();
                c.at = 9;
                c
            }),
            "same detail re-run → no-op"
        );
    }

    // ================= FU1 — ping-on-tranch =================

    // FU1 ⭐ the notify TARGETS the pusher + tichef with the verdict, and NEVER
    // fans out. The pure core with injected seams (unit-level).
    #[test]
    fn fu1_tranch_notify_targets_pusher_and_tichef_never_fans_out() {
        use std::cell::RefCell;
        // (1) a known pusher → pusher + tichef, both pinged, message carries verdict+from.
        let note = RefCell::new(None::<(String, String, String)>);
        let pinged = RefCell::new(Vec::<(String, String)>::new());
        let report = emit_tranch_notify(
            "ra1c",
            "GO",
            Some("team-back"),
            |topic, from, msg| *note.borrow_mut() = Some((topic.into(), from.into(), msg.into())),
            |target, msg| {
                pinged.borrow_mut().push((target.into(), msg.into()));
                Ok(())
            },
        );
        let (topic, from, msg) = note.borrow().clone().expect("a durable note was posted");
        assert_eq!(topic, "decisions", "the pull-friendly log is the `decisions` topic");
        assert_eq!(from, "PO", "the ruler is the PO");
        assert_eq!(msg, "DECISION_TRANCHED id=ra1c verdict=GO from=team-back by=PO", "note carries id+verdict+pusher");
        let targets: Vec<String> = pinged.borrow().iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(targets, vec!["team-back", "tichef"], "CIBLÉ: pusher + tichef, exactly — never a fan-out");
        assert!(pinged.borrow().iter().all(|(_, m)| m == &msg), "the ping carries the same DECISION_TRANCHED marker");
        assert_eq!(report.pinged, vec!["team-back", "tichef"]);
        assert_eq!(report.failed, 0);

        // (2) no pusher (old event / no --from) → only tichef; from=- in the payload.
        let solo = RefCell::new(Vec::<String>::new());
        let r2 = emit_tranch_notify("d", "NO-GO", None, |_t, _f, _m| {}, |t, _m| {
            solo.borrow_mut().push(t.into());
            Ok(())
        });
        assert_eq!(*solo.borrow(), vec!["tichef"], "unknown pusher → tichef only (still no fan-out)");
        assert_eq!(r2.pinged, vec!["tichef"]);
        assert_eq!(tranch_notify_msg("d", "NO-GO", None), "DECISION_TRANCHED id=d verdict=NO-GO from=- by=PO");

        // (3) a pusher that IS tichef is pinged ONCE (de-dup, still no fan-out).
        assert_eq!(tranch_targets(Some("tichef")), vec!["tichef"], "pusher==tichef → single ping");

        // A failing ping is COUNTED, never propagated (best-effort; the note is the fallback).
        let r3 = emit_tranch_notify("d", "GO", Some("A"), |_t, _f, _m| {}, |_t, _m| Err(std::io::Error::other("no daemon")));
        assert_eq!(r3.failed, 2, "both pings failed…");
        assert!(r3.pinged.is_empty(), "…none reached");
        assert!(r3.note_posted, "…but the durable note was still attempted (the pull fallback)");
    }

    // FU1 ⭐ ANTI-BUILT≠WIRED (real-fs, no mock): `decision push --from A` then a
    // `tranch` fires `notify_on_tranch`, which (1) READS A back from the log,
    // (2) posts a real `DECISION_TRANCHED … from=A` note to a real blackboard file,
    // (3) enqueues real submit=true swamp pings to A + tichef — round-tripped through
    // the actual note/swamp serialization — WHILE (4) `decision list` still shows the
    // tranched state (the passive log is untouched: NO regression).
    #[test]
    fn fu1_tranch_path_notifies_and_leaves_the_passive_log_intact_on_real_fs() {
        use crate::cli::aligator::{Priority, parse_swamp};
        use crate::cli::team::parse_notes;

        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let log = TmpLog::new();
        let blackboard = std::env::temp_dir().join(format!("fu1-bb-{}.jsonl", crate::default_tab_id()));
        let swamp = std::env::temp_dir().join(format!("fu1-swamp-{}.jsonl", crate::default_tab_id()));
        let _c1 = Rm(blackboard.clone());
        let _c2 = Rm(swamp.clone());

        // `decision push --from A` (the pusher is recorded on the open event) → tranch.
        let mut o = open("ra1c", "harness", "Deploy RA1c", 100);
        o.from = Some("A".into());
        push_event(log.path(), &o, false).unwrap();
        let mut tr = ev("ra1c", DecisionKind::Tranched, 120);
        tr.verdict = Some("GO".into());
        append_line(log.path(), &tr).unwrap();

        // The tranch → notify STEP the CLI runs, with real-fs seams (temp blackboard +
        // temp swamp) instead of the live daemon. This is the exact wiring `mark` calls.
        let report = notify_on_tranch(
            log.path(),
            "ra1c",
            "GO",
            // note seam = append a real Note line (the real serialization) to a temp file.
            |topic, from, msg| {
                use std::io::Write as _;
                let n = crate::cli::team::Note { ts: 120, from: Some(from.into()), topic: Some(topic.into()), msg: msg.into() };
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&blackboard).unwrap();
                f.write_all(crate::cli::team::encode_note_line(&n).as_bytes()).unwrap();
            },
            // ping seam = the REAL regulated swamp push (submit=true), path-injected.
            |target, msg| {
                crate::cli::restart_wake::push_swamp_input(
                    &swamp,
                    target,
                    msg,
                    true,
                    120_000,
                    Priority::Status,
                    Some(format!("decision-tranched-ra1c-120-{target}")),
                )
            },
        );
        assert_eq!(report.pinged, vec!["A", "tichef"], "the pusher READ FROM THE LOG (A) + tichef were pinged");
        assert_eq!(report.failed, 0);

        // (2) the durable DECISION_TRANCHED note really landed on the `decisions` topic.
        let notes = parse_notes(&std::fs::read_to_string(&blackboard).unwrap());
        let tranched: Vec<_> = notes.iter().filter(|n| n.topic.as_deref() == Some("decisions")).collect();
        assert_eq!(tranched.len(), 1, "exactly one DECISION_TRANCHED note (no fan-out)");
        assert_eq!(tranched[0].msg, "DECISION_TRANCHED id=ra1c verdict=GO from=A by=PO", "from=A read back from --from + verdict");

        // (3) the swamp really carries two submit=true pings to A + tichef (the active wake).
        let entries = parse_swamp(&std::fs::read_to_string(&swamp).unwrap());
        assert_eq!(entries.len(), 2, "pusher + tichef — CIBLÉ, never a fan-out");
        assert_eq!(entries.iter().map(|e| e.tab.as_str()).collect::<Vec<_>>(), vec!["A", "tichef"]);
        for e in &entries {
            assert!(e.submit, "⭐ the ping SUBMITS (triggers the target's turn — the active notify)");
            assert_eq!(e.priority, Priority::Status);
            assert!(e.input.starts_with("DECISION_TRANCHED"), "carries the tranch marker");
            assert!(e.dedup_key.as_deref().unwrap().starts_with("decision-tranched-ra1c-120-"), "deduped per (decision,ts,target)");
        }
        // Distinct dedup keys per target → aligator delivers BOTH (not deduped as one).
        assert_ne!(entries[0].dedup_key, entries[1].dedup_key, "per-target keys so the 2nd ping isn't dropped as a dup");

        // (4) NO REGRESSION: the passive `decision list` still shows the tranched state.
        let views = read_decisions_at(log.path(), false);
        let d = views.iter().find(|d| d.id == "ra1c").expect("still listed (tranched, not yet archived)");
        assert_eq!(d.state, DecisionState::Tranched, "the passive log is intact — state readable by all");
        assert_eq!(d.verdict.as_deref(), Some("GO"), "and its verdict");
    }

    // FU1: `--from` rides the fold like any content field, latest-content-wins, and a
    // re-push with a DIFFERENT pusher is a real content change (not a no-op).
    #[test]
    fn fu1_pusher_rides_the_content_axis_and_is_read_back() {
        let log = TmpLog::new();
        let mut o = open("d", "harness", "T", 1);
        o.from = Some("A".into());
        append_line(log.path(), &o).unwrap();
        let events: Vec<DecisionEvent> = parse_decisions(&std::fs::read_to_string(log.path()).unwrap()).into_iter().filter(|e| e.id == "d").collect();
        assert_eq!(pusher_of(&events).as_deref(), Some("A"), "the pusher is read back from the stored --from");

        // A decision without --from → no pusher (rétro-compat).
        let plain: Vec<DecisionEvent> = vec![open("p", "harness", "T", 1)];
        assert_eq!(pusher_of(&plain), None, "no --from → no pusher (only tichef would be notified)");

        // `from` is part of the idempotence key: a re-push by a different agent appends.
        let a = { let mut e = open("d", "harness", "T", 1); e.from = Some("A".into()); e };
        let b = { let mut e = open("d", "harness", "T", 2); e.from = Some("B".into()); e };
        assert!(!is_noop_open(std::slice::from_ref(&a), &b), "a different pusher is a real content change");
        let same = { let mut e = open("d", "harness", "T", 9); e.from = Some("A".into()); e };
        assert!(is_noop_open(std::slice::from_ref(&a), &same), "same pusher + content → still a no-op");
    }

    // Item 2 (#kiosk): the NEW `summary` field rides the content axis, folds into the view,
    // and is part of the idempotence key (a summary change is a real content change).
    #[test]
    fn summary_rides_content_axis_and_is_part_of_idempotence() {
        let log = TmpLog::new();
        let mut o = open("d", "harness", "Titre gras", 1);
        o.summary = Some("Un résumé 2-3 lignes\nsous le titre.".into());
        append_line(log.path(), &o).unwrap();
        let v = read_decisions_at(log.path(), false);
        let d = v.iter().find(|d| d.id == "d").unwrap();
        assert_eq!(d.title.as_deref(), Some("Titre gras"), "title kept (NOT renamed)");
        assert_eq!(
            d.summary.as_deref(),
            Some("Un résumé 2-3 lignes\nsous le titre."),
            "summary folds into the view, distinct from title"
        );
        let same = { let mut e = open("d", "harness", "Titre gras", 9); e.summary = o.summary.clone(); e };
        assert!(is_noop_open(std::slice::from_ref(&o), &same), "same summary → no-op");
        let changed = { let mut e = open("d", "harness", "Titre gras", 9); e.summary = Some("autre résumé".into()); e };
        assert!(!is_noop_open(std::slice::from_ref(&o), &changed), "summary change → appends");
    }

    // Item 5 (#kiosk): `compose` assembles a DETERMINISTIC detail — fixed section order,
    // each --command auto-fenced (so the Kiosk shows the 📋 copy button), options as a list.
    #[test]
    fn compose_detail_is_deterministic_and_fences_commands() {
        let args: Vec<String> = [
            "--enjeux", "perte upstream",
            "--options", "A: PR maintenant",
            "--options", "B: attendre",
            "--reco", "A",
            "--effort", "30 min",
            "--command", "git push origin feat/x",
            "--link", "https://ex/report",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let detail = compose_detail(&args);
        let i_enjeux = detail.find("**Enjeux**").expect("enjeux section");
        let i_options = detail.find("**Options**").expect("options section");
        let i_reco = detail.find("**Reco**").expect("reco section");
        let i_effort = detail.find("**Effort**").expect("effort section");
        assert!(i_enjeux < i_options && i_options < i_reco && i_reco < i_effort, "deterministic order");
        assert!(detail.contains("- A: PR maintenant") && detail.contains("- B: attendre"), "options list");
        assert!(detail.contains("```sh\ngit push origin feat/x\n```"), "command auto-fenced verbatim");
        assert!(detail.contains("**Lien** — https://ex/report"), "link in footer");
        assert_eq!(compose_detail(&["--id".to_string(), "x".to_string()]), "", "no sections → empty detail");
    }
}
