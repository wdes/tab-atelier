// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `task` — a typed task queue with an ATOMIC claim (primitive #11, POC).
//!
//! Scope so far: the file+append backend (reused from aligator's swamp: append +
//! compaction, `src/cli/aligator.rs`) + `push`/`claim`/`beat`/`done`, the atomic
//! claim serialized behind the mono-process daemon (S1), lease/beat/orphan-
//! reclaim (S2), and capacity `--to <role>` gating the claim (S3). NO read-model
//! (S4).
//!
//! **Founding insight** (design `issue-task-queue-lease.md`): the daemon is a
//! single process, so a `claim` handled behind the API is a free compare-and-
//! swap — the daemon (under a lock, like `reset-master-token`) is the only
//! writer, so two concurrent claims serialize and exactly one wins.
//!
//! **Delivery semantics (consumer-side)**: the lease buys AT-LEAST-ONCE
//! EXECUTION, EXACTLY-ONCE COMPLETION. If a claimer dies mid-task (no `beat`
//! within the TTL), its lease expires and the task is re-claimed → the work may
//! RE-EXECUTE on another peer. Only COMPLETION is exactly-once: a `done` prunes
//! the entry so it never re-appears, and a late `done` from the orphaned claimer
//! is refused (stale). Consumers must make task execution idempotent (or tolerate
//! re-runs); the lease bounds the worst case, it doesn't prevent re-execution.
//!
//! **Capacity vs ownership (S3 — keep them SEPARATE)**: `--to <role>` gates the
//! CLAIM by ROLE (a task's required capability, matched against the caller's card
//! role); OWNERSHIP of a claimed task (`beat`/`done`) is by the claimer's TAB-ID
//! (`claimed_by`), never the role. So two peers of the SAME role may both claim,
//! but once one wins, only its tab-id can `beat`/`done` it — the other (same
//! role, different tab-id) is refused stale. Capacity gates the claim; tab-id
//! gates the ownership.
//!
//! This module holds the PURE backend (the data model, the selection/claim/done/
//! compact logic, and parse/encode) so the atomic-case invariants are unit-tested
//! without a live daemon; the file I/O and route wiring add the effects on top.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A task's lifecycle state. `queued` → `claimed` (by a peer) → `done`; a `done`
/// entry is pruned at the next compaction (exactly-once: it never re-appears).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Claimed,
    Done,
}

/// One task in a queue file (`<state>/tab-atelier/tasks/<queue>.jsonl`), one
/// JSON object per line — mirrors `SwampEntry`'s append model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEntry {
    /// Unix seconds when enqueued.
    pub ts: u64,
    /// Stable task id (a UUID, like tab ids) — the handle for `done`.
    pub id: String,
    /// The queue this task belongs to.
    pub queue: String,
    /// Opaque payload the claimer receives.
    pub payload: String,
    /// Drain priority: higher = more urgent, claimed first (default 0).
    #[serde(default)]
    pub priority: u8,
    /// Lifecycle state.
    pub state: TaskState,
    /// S2 — who currently holds the claim (`None` while `Queued`). `beat`/`done`
    /// prove ownership against this. Omitted from the JSONL while unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// S2 — unix-millis the claim's lease expires at (`None` while `Queued`).
    /// Past it (no `beat`), the task is reclaimable → orphan recovery. Omitted
    /// while unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<u64>,
    /// S3 — the ROLE required to claim this task (`--to <role>`), a CAPACITY gate
    /// on the claim only. `None` = claimable by any role. Matched against the
    /// caller's card role (assignment/specialty); it does NOT affect ownership,
    /// which stays on `claimed_by` (the tab-id). Omitted when unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Default lease if `--lease` isn't given: ~35 min, generalising the
/// heartbeat/TTL of `triage-tickets` (H=15 min / TTL=35 min).
pub const DEFAULT_LEASE_MS: u64 = 35 * 60 * 1000;

/// The directory holding the per-queue files. Honors `TAB_ATELIER_TASKS_DIR`
/// (a test seam so integration tests isolate onto a tmpdir); production uses
/// `<state>/tab-atelier/tasks`.
#[must_use]
pub fn tasks_dir() -> PathBuf {
    if let Ok(d) = std::env::var("TAB_ATELIER_TASKS_DIR") {
        return PathBuf::from(d);
    }
    crate::platform::state_base_dir().join("tab-atelier").join("tasks")
}

/// Every queue file currently on disk (`*.jsonl` under [`tasks_dir`]). Used by
/// `done`, which addresses a task by id alone and must find its queue.
#[must_use]
pub fn queue_files() -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(tasks_dir()) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect()
}

/// The append-only file for one queue. Queue names are sanitised to a single
/// path component so a malicious `--queue ../../etc` can't escape the dir.
#[must_use]
pub fn queue_path(queue: &str) -> PathBuf {
    let safe: String = queue
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    tasks_dir().join(format!("{safe}.jsonl"))
}

/// Parse a queue file body into entries, skipping blank / unparseable lines (a
/// half-written line from a racing appender is dropped, not fatal — same
/// tolerance as `parse_swamp`).
#[must_use]
pub fn parse_tasks(body: &str) -> Vec<TaskEntry> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<TaskEntry>(l).ok())
        .collect()
}

/// One task entry as a JSONL line (trailing newline included).
#[must_use]
pub fn encode_task_line(e: &TaskEntry) -> String {
    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// Is this entry claimable at `now`?
///
/// `Queued`, OR `Claimed` with an EXPIRED (or missing) lease — the S2 orphan
/// reclamation: a peer that claimed then died (no `beat` within the TTL) has its
/// task freed for another idle peer. A fresh claim (`lease_until` in the future)
/// is NOT claimable. Pure.
#[must_use]
pub fn is_claimable(e: &TaskEntry, now: u64) -> bool {
    match e.state {
        TaskState::Queued => true,
        // A claim with no lease (legacy / never leased) or an expired one is an
        // orphan → reclaimable. A live lease (`lease_until >= now`) is not.
        TaskState::Claimed => e.lease_until.is_none_or(|l| l < now),
        TaskState::Done => false,
    }
}

/// Does `caller_role` satisfy a task's `to` requirement? (S3 capacity gate.)
///
/// A task with no `to` (`None`) is claimable by ANY role; a `--to <role>` task is
/// claimable ONLY by a caller whose role matches exactly. This gates the CLAIM
/// only — ownership (`beat`/`done`) is by tab-id, never the role.
#[must_use]
pub fn role_matches(required: Option<&str>, caller_role: &str) -> bool {
    required.is_none_or(|r| r == caller_role)
}

/// The index of the next task `caller_role` may claim at `now`.
///
/// The first entry that is both [`is_claimable`] AND [`role_matches`] (S3
/// capacity), ordered by priority DESC then FIFO (oldest `ts` first, then file
/// order for a stable tie-break). `None` when nothing is claimable by this role.
/// Pure — the atomic serialization is the caller's (the daemon lock).
#[must_use]
pub fn select_next_claim(entries: &[TaskEntry], now: u64, caller_role: &str) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_claimable(e, now) && role_matches(e.to.as_deref(), caller_role))
        .min_by(|(ia, a), (ib, b)| {
            b.priority
                .cmp(&a.priority) // higher priority first
                .then(a.ts.cmp(&b.ts)) // then oldest first (FIFO)
                .then(ia.cmp(ib)) // then file order (stable)
        })
        .map(|(i, _)| i)
}

/// Claim the next task for `claimer` (tab-id) acting as `caller_role`, at `now`,
/// with a `lease_ms` lease.
///
/// Flip the selected entry — claimable (queued OR expired-claim = orphan) AND
/// role-matching (S3 capacity) — to `Claimed`, stamp `claimed_by = claimer` and
/// `lease_until = now + lease_ms`, and return a clone. `None` when nothing is
/// claimable by this role. Under the daemon lock, two concurrent claims serialize
/// → exactly one wins. `claimer` is the tab-id (ownership), `caller_role` the
/// capacity — kept SEPARATE.
#[must_use]
pub fn claim_next(
    entries: &mut [TaskEntry],
    now: u64,
    claimer: &str,
    caller_role: &str,
    lease_ms: u64,
) -> Option<TaskEntry> {
    let idx = select_next_claim(entries, now, caller_role)?;
    entries[idx].state = TaskState::Claimed;
    entries[idx].claimed_by = Some(claimer.to_string());
    entries[idx].lease_until = Some(now.saturating_add(lease_ms));
    Some(entries[idx].clone())
}

/// The verdict of a `done` attempt (S2, lease-aware).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneOutcome {
    /// Flipped a live-lease claim held by the caller → `Done` (200).
    Completed,
    /// Already `Done` (a repeat by the same claimer) → idempotent 200.
    AlreadyDone,
    /// The caller isn't the current claimer, or its lease expired / the task was
    /// re-claimed → refuse (409). A late `done` from an orphaned claimer.
    Stale,
    /// No such id (the queue never held it / it was compacted) → treated as 200
    /// idempotent by the caller (nothing to complete, nothing stale to protect).
    NotFound,
}

/// Complete task `id` on behalf of `claimer` at `now` (S2, lease-checked).
///
/// A `done` is honoured ONLY while the caller still holds a VALID lease:
/// `claimed_by == claimer` AND `lease_until >= now`. Otherwise it's [`Stale`] —
/// a late `done` from a claimer whose lease expired (and whose task may have
/// been re-claimed) can't complete work another peer now owns. An already-`Done`
/// task by the same claimer is idempotent.
///
/// [`Stale`]: DoneOutcome::Stale
#[must_use]
pub fn done_task(entries: &mut [TaskEntry], id: &str, claimer: &str, now: u64) -> DoneOutcome {
    let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
        return DoneOutcome::NotFound;
    };
    match e.state {
        // Idempotent only for the claimer that actually completed it.
        TaskState::Done => {
            if e.claimed_by.as_deref() == Some(claimer) {
                DoneOutcome::AlreadyDone
            } else {
                DoneOutcome::Stale
            }
        }
        TaskState::Claimed => {
            let owns = e.claimed_by.as_deref() == Some(claimer);
            let live = e.lease_until.is_some_and(|l| l >= now);
            if owns && live {
                e.state = TaskState::Done;
                DoneOutcome::Completed
            } else {
                // Wrong claimer, expired lease, or re-claimed → stale.
                DoneOutcome::Stale
            }
        }
        // A queued task was never claimed by anyone → a done for it is stale.
        TaskState::Queued => DoneOutcome::Stale,
    }
}

/// Renew the lease on task `id` for `claimer` at `now` (S2 heartbeat).
///
/// Only the CURRENT claimer (`claimed_by == claimer`) of a still-`Claimed` task
/// can `beat`; it pushes `lease_until` to `now + lease_ms`, keeping the claim
/// alive past the TTL so it isn't reclaimed. Returns `true` on success, `false`
/// if the id is unknown, not claimed, or held by someone else (already
/// reclaimed). Pure.
#[must_use]
pub fn beat_task(entries: &mut [TaskEntry], id: &str, claimer: &str, now: u64, lease_ms: u64) -> bool {
    if let Some(e) = entries.iter_mut().find(|e| e.id == id)
        && e.state == TaskState::Claimed
        && e.claimed_by.as_deref() == Some(claimer)
    {
        e.lease_until = Some(now.saturating_add(lease_ms));
        return true;
    }
    false
}

/// Entries a compaction KEEPS: everything not `Done`. The `Done` entries are
/// pruned, so a completed task can never be re-claimed (exactly-once) — the same
/// bounded-file discipline as aligator's `compact`.
#[must_use]
pub fn compact_tasks(entries: &[TaskEntry]) -> Vec<TaskEntry> {
    entries.iter().filter(|e| e.state != TaskState::Done).cloned().collect()
}

/// The claim window's read-modify-write, PURE.
///
/// Claim the next task AND compact (prune `Done`) in the SAME pass — aligator's
/// parity: it compacts at the drain, we compact at the claim, so the file stays
/// BOUNDED instead of accumulating `done` entries without limit (and S2's
/// heartbeats would only make that worse). Exactly-once is preserved: a pruned
/// `done` is gone from the file, so it can never re-appear in a later claim.
///
/// Returns `(claimed, kept, changed)`: the claimed task (if any), the entries to
/// persist, and whether anything changed (so the caller can skip a no-op write).
#[must_use]
pub fn claim_and_compact(
    mut entries: Vec<TaskEntry>,
    now: u64,
    claimer: &str,
    caller_role: &str,
    lease_ms: u64,
) -> (Option<TaskEntry>, Vec<TaskEntry>, bool) {
    let before = entries.len();
    let claimed = claim_next(&mut entries, now, claimer, caller_role, lease_ms);
    let kept = compact_tasks(&entries);
    // A write is needed if we claimed (a state flipped) or pruned any done entry.
    let changed = claimed.is_some() || kept.len() != before;
    (claimed, kept, changed)
}

/// Append one task line to a queue file (create + append, line-atomic like the
/// swamp producer). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / write I/O error.
pub fn append_task_line(path: &Path, entry: &TaskEntry) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_task_line(entry).as_bytes())
}

/// Perform one claim window: select+compact, then persist via `persist` — and
/// hand the task out ONLY if that persist SUCCEEDED.
///
/// The exactly-once-under-fs-failure guard (Olympe's catch). `persist` is a
/// closure (the caller injects `write_tasks_atomic`; tests inject a failing
/// write) applied to the entries to keep. A claim is **won** only when a task was
/// claimed AND `persist` returned `Ok`. If persist FAILS → return `None`: the
/// on-disk file is unchanged (the claim was only in the in-memory copy), so the
/// task is still `queued` → still claimable; a later claim retries once the fs
/// recovers. NOT handing out a task is safe; handing one out without a durable
/// claim record would let another claimer re-take the still-`queued` entry = a
/// silent DOUBLE-CLAIM. (A claim always sets `changed`, so it always passes the
/// persist gate.)
#[must_use]
pub fn perform_claim<F>(
    entries: Vec<TaskEntry>,
    now: u64,
    claimer: &str,
    caller_role: &str,
    lease_ms: u64,
    persist: F,
) -> Option<TaskEntry>
where
    F: FnOnce(&[TaskEntry]) -> std::io::Result<()>,
{
    let (claimed, kept, changed) = claim_and_compact(entries, now, claimer, caller_role, lease_ms);
    if changed && persist(&kept).is_err() {
        // Persist failed → do NOT hand out the task (it stays claimable on disk).
        return None;
    }
    claimed
}

/// Rewrite a queue file to exactly `entries`, atomically (tmp + rename), like
/// `compact_swamp`. Used by the claim/done read-modify-write and by compaction.
///
/// Unlike aligator's swamp — where an EXTERNAL `tab-atelier swamp` producer
/// appends concurrently, so `compact_swamp` warns a racing append can be lost —
/// EVERY task writer (push AND claim AND done) runs under the daemon's snapshot
/// lock, so there is no concurrent writer to race this read-modify-write: the
/// producer race is CLOSED here. ponytail: that holds only while the daemon is
/// the sole writer; if an external writer is ever added (a CLI writing the file
/// directly, or a second process), an flock is the upgrade path.
///
/// # Errors
/// Propagates any create-dir / write / rename I/O error.
pub fn write_tasks_atomic(path: &Path, entries: &[TaskEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body: String = entries.iter().map(encode_task_line).collect();
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// CLI (thin HTTP client): `task <push|claim|done>` POSTs to the local daemon so
// the CLAIM is serialized behind the API (the whole point). Reuses the endpoint
// discovery + ureq agent aligator/share_link already use.
// ---------------------------------------------------------------------------

/// `tab-atelier task <push|claim|beat|done> …` — the producer/consumer CLI.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("push") => run_push(&args[1..]),
        Some("claim") => run_claim(&args[1..]),
        Some("beat") => run_beat(&args[1..]),
        Some("done") => run_done(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  tab-atelier task push --queue <q> [--to <role>] [--priority N] \"<payload>\"\n  \
                 tab-atelier task claim --queue <q> [--as <role>] [--lease <secs>]\n  \
                 tab-atelier task beat <task-id> [--lease <secs>]\n  \
                 tab-atelier task done <task-id>"
            );
            2
        }
    }
}

/// The caller's OWNERSHIP identity (tab-id): env `_TAB_ID` (the tab the daemon
/// injects), else `"anon"`. `beat`/`done` prove ownership against this, so a
/// worker beats/completes only its OWN tasks. NOT the role — capacity (`--as`)
/// and ownership (this) are kept separate (S3 garde-fou).
fn caller_tab_id() -> String {
    std::env::var("_TAB_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anon".to_string())
}

/// POST to `<api>/task/<path>` with a Bearer + JSON body, returning
/// `(status, body)` or an error string. Reuses `share_link`'s endpoint + agent.
fn post_task(path: &str, body: &str) -> Result<(u16, String), String> {
    let ep = crate::cli::share_link::discover_endpoint()?;
    let mut resp = crate::cli::share_link::agent()
        .post(format!("{}/task/{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| format!("POST /task/{path}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((status, text))
}

/// `--queue <q>` extractor shared by push/claim.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn run_push(args: &[String]) -> i32 {
    let Some(queue) = arg_value(args, "--queue") else {
        eprintln!("task push: --queue <q> required");
        return 2;
    };
    let priority: u8 = arg_value(args, "--priority").and_then(|s| s.parse().ok()).unwrap_or(0);
    // Payload = the first positional (non-flag, not consumed by a flag).
    let mut skip = false;
    let payload = args.iter().find(|a| {
        if skip {
            skip = false;
            return false;
        }
        if a.starts_with("--") {
            skip = matches!(a.as_str(), "--queue" | "--priority");
            return false;
        }
        true
    });
    let Some(payload) = payload else {
        eprintln!("task push: a \"<payload>\" is required");
        return 2;
    };
    let mut body = serde_json::json!({ "payload": payload, "priority": priority });
    // S3 — an optional `--to <role>` capacity requirement on the task.
    if let Some(role) = arg_value(args, "--to") {
        body["to"] = role.into();
    }
    match post_task(&format!("{queue}/push"), &body.to_string()) {
        Ok((201, text)) => {
            println!("{text}");
            0
        }
        Ok((code, text)) => {
            eprintln!("task push: HTTP {code}: {text}");
            1
        }
        Err(e) => {
            eprintln!("task push: {e}");
            1
        }
    }
}

fn run_claim(args: &[String]) -> i32 {
    let Some(queue) = arg_value(args, "--queue") else {
        eprintln!("task claim: --queue <q> required");
        return 2;
    };
    // Ownership = tab-id (claimed_by); capacity = role (`--as`, else the daemon
    // reads it off the caller's card). Kept separate (S3 garde-fou).
    let mut body = serde_json::json!({ "claimed_by": caller_tab_id() });
    if let Some(role) = arg_value(args, "--as") {
        body["role"] = role.into();
    }
    if let Some(secs) = arg_value(args, "--lease").and_then(|s| s.parse::<u64>().ok()) {
        body["lease_secs"] = secs.into();
    }
    match post_task(&format!("{queue}/claim"), &body.to_string()) {
        // 200 → a task; print {id,payload,lease_until}. 204 → empty; print nothing.
        Ok((200, text)) => {
            println!("{text}");
            0
        }
        Ok((204, _)) => 0,
        Ok((code, text)) => {
            eprintln!("task claim: HTTP {code}: {text}");
            1
        }
        Err(e) => {
            eprintln!("task claim: {e}");
            1
        }
    }
}

/// The `<task-id>` positional shared by beat/done — the first non-flag arg that
/// isn't the value consumed by `--as`/`--lease`.
fn task_id_arg(args: &[String]) -> Option<&str> {
    let mut skip = false;
    args.iter()
        .find(|a| {
            if skip {
                skip = false;
                return false;
            }
            if a.starts_with("--") {
                skip = matches!(a.as_str(), "--as" | "--lease");
                return false;
            }
            true
        })
        .map(String::as_str)
}

fn run_beat(args: &[String]) -> i32 {
    let Some(id) = task_id_arg(args) else {
        eprintln!("task beat: a <task-id> is required");
        return 2;
    };
    let mut body = serde_json::json!({ "claimed_by": caller_tab_id() });
    if let Some(secs) = arg_value(args, "--lease").and_then(|s| s.parse::<u64>().ok()) {
        body["lease_secs"] = secs.into();
    }
    match post_task(&format!("{id}/beat"), &body.to_string()) {
        // 200 → renewed (prints {lease_until}). 409 → lease lost / not the owner.
        Ok((200, text)) => {
            println!("{text}");
            0
        }
        Ok((code, text)) => {
            eprintln!("task beat: HTTP {code}: {text}");
            1
        }
        Err(e) => {
            eprintln!("task beat: {e}");
            1
        }
    }
}

fn run_done(args: &[String]) -> i32 {
    let Some(id) = task_id_arg(args) else {
        eprintln!("task done: a <task-id> is required");
        return 2;
    };
    let body = serde_json::json!({ "claimed_by": caller_tab_id() }).to_string();
    match post_task(&format!("{id}/done"), &body) {
        Ok((200, _)) => 0,
        Ok((code, text)) => {
            eprintln!("task done: HTTP {code}: {text}");
            1
        }
        Err(e) => {
            eprintln!("task done: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000; // a fixed "now" (unix-millis) for deterministic leases
    const LEASE: u64 = 35 * 60 * 1000; // 35 min

    fn task(id: &str, priority: u8, state: TaskState) -> TaskEntry {
        TaskEntry {
            ts: 0,
            id: id.into(),
            queue: "q".into(),
            payload: format!("payload-{id}"),
            priority,
            state,
            claimed_by: None,
            lease_until: None,
            to: None,
        }
    }

    /// A task requiring role `to` to claim (S3 capacity fixture).
    fn task_to(id: &str, to: &str) -> TaskEntry {
        TaskEntry {
            to: Some(to.into()),
            ..task(id, 0, TaskState::Queued)
        }
    }

    // Acceptance #1 (task-s1-build-spec §Acceptance): two concurrent claims on a
    // one-task queue → EXACTLY ONE gets it. The daemon serializes concurrent
    // claims (single-writer under the lock), so "concurrent" == "sequential
    // claim_next on the same, mutating vec": the 2nd sees the entry already
    // Claimed (live lease) → empty. Never both.
    #[test]
    fn claim_is_exclusive_under_concurrency() {
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        let a = claim_next(&mut q, NOW, "peer-a", "", LEASE);
        let b = claim_next(&mut q, NOW, "peer-b", "", LEASE); // the serialized second claimer
        assert_eq!(a.as_ref().map(|t| t.id.as_str()), Some("t1"), "one claimer wins");
        assert!(b.is_none(), "the other claimer gets nothing — never both");
        assert_eq!(q[0].state, TaskState::Claimed, "the task is claimed exactly once");
        assert_eq!(
            q[0].claimed_by.as_deref(),
            Some("peer-a"),
            "claimed_by records the winner"
        );
        assert_eq!(q[0].lease_until, Some(NOW + LEASE), "the claim stamps a lease");
    }

    // Acceptance #4: a `done` task NEVER re-appears in a later claim.
    #[test]
    fn done_task_never_reappears() {
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        let claimed = claim_next(&mut q, NOW, "peer-a", "", LEASE).expect("claimed");
        assert_eq!(
            done_task(&mut q, &claimed.id, "peer-a", NOW),
            DoneOutcome::Completed,
            "owner completes it"
        );
        // Compaction prunes the done entry (bounded file), and a claim after it
        // finds nothing — the done task is gone for good.
        let q = compact_tasks(&q);
        assert!(q.is_empty(), "done task pruned at compaction");
        let mut q = q;
        assert!(
            claim_next(&mut q, NOW, "peer-b", "", LEASE).is_none(),
            "a done task never re-appears in a claim"
        );
        // A done for a pruned/unknown id → NotFound (caller treats as idempotent 200).
        assert_eq!(
            done_task(&mut q, "t1", "peer-a", NOW),
            DoneOutcome::NotFound,
            "done on a gone id → NotFound"
        );
    }

    // Acceptance #2: orphan reclamation + stale-done. A claim whose lease EXPIRES
    // (no `beat` within the TTL) becomes claimable again → an idle peer reclaims
    // it; a late `done` from the original, now-orphaned claimer is refused (Stale
    // → the handler maps it to 409).
    #[test]
    fn expired_lease_is_reclaimed_and_late_done_is_stale() {
        const SHORT: u64 = 1000; // a 1s lease, to expire it deterministically
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        let first = claim_next(&mut q, NOW, "peer-a", "", SHORT).expect("peer-a claims");
        assert_eq!(q[0].claimed_by.as_deref(), Some("peer-a"));
        // Before the lease expires, the task is NOT claimable (a live claim).
        assert!(!is_claimable(&q[0], NOW), "a live lease is not reclaimable");
        // Past the lease (no beat) → orphan → claimable again.
        let now2 = NOW + SHORT + 1;
        assert!(is_claimable(&q[0], now2), "an expired lease is reclaimable");
        let second = claim_next(&mut q, now2, "peer-b", "", LEASE).expect("peer-b reclaims the orphan");
        assert_eq!(second.id, first.id, "same task, reclaimed by another peer");
        assert_eq!(q[0].claimed_by.as_deref(), Some("peer-b"), "peer-b now owns it");
        // The original, orphaned claimer's late `done` can't complete work peer-b
        // now owns → Stale.
        assert_eq!(
            done_task(&mut q, &first.id, "peer-a", now2),
            DoneOutcome::Stale,
            "a late done from the orphaned claimer is stale"
        );
        // The live owner (peer-b) still completes it.
        assert_eq!(
            done_task(&mut q, &first.id, "peer-b", now2),
            DoneOutcome::Completed,
            "the live owner completes it"
        );
    }

    // Acceptance #3: a `beat` keeps a claim alive past the ORIGINAL TTL — no
    // untimely reclamation. Only the owner can beat; a non-owner (or an unknown
    // id) is a no-op.
    #[test]
    fn beat_keeps_the_claim_alive_past_the_original_lease() {
        const SHORT: u64 = 1000;
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        claim_next(&mut q, NOW, "peer-a", "", SHORT).expect("peer-a claims");
        // Renew before expiry → the lease pushes to a later window.
        let mid = NOW + SHORT / 2;
        assert!(
            beat_task(&mut q, "t1", "peer-a", mid, LEASE),
            "the owner renews its lease"
        );
        assert_eq!(q[0].lease_until, Some(mid + LEASE), "the lease is pushed forward");
        // A `now` past the ORIGINAL lease but within the renewed one → NOT reclaimable.
        let past_original = NOW + SHORT + 1;
        assert!(
            !is_claimable(&q[0], past_original),
            "the renewed lease keeps the claim alive past the original TTL"
        );
        // A non-owner cannot beat (it doesn't hold the claim), nor can an unknown id.
        assert!(
            !beat_task(&mut q, "t1", "peer-b", past_original, LEASE),
            "a non-owner's beat is refused"
        );
        assert!(
            !beat_task(&mut q, "nope", "peer-a", past_original, LEASE),
            "a beat on an unknown id is a no-op"
        );
    }

    // Acceptance #5: capacity gate. A `--to <role>` task is claimable ONLY by a
    // caller whose role matches; another role gets nothing (204 at the API). A
    // task with no `to` stays claimable by everyone (S1/S2 unchanged).
    #[test]
    fn capacity_to_gates_the_claim_by_role() {
        // role_matches: the pure predicate the gate is built on.
        assert!(role_matches(None, "anyone"), "no requirement → any role claims");
        assert!(role_matches(Some("builder"), "builder"), "matching role claims");
        assert!(!role_matches(Some("builder"), "reviewer"), "wrong role can't claim");
        assert!(
            !role_matches(Some("builder"), ""),
            "no role can't claim a restricted task"
        );

        let mut q = vec![task_to("t1", "builder")];
        // A reviewer can't claim a builder task → nothing selected.
        assert!(
            claim_next(&mut q, NOW, "tab-rev", "reviewer", LEASE).is_none(),
            "a mismatched role claims nothing"
        );
        assert_eq!(q[0].state, TaskState::Queued, "the task is untouched, still queued");
        // A builder claims it.
        let got = claim_next(&mut q, NOW, "tab-bld", "builder", LEASE).expect("builder claims");
        assert_eq!(got.id, "t1");
        assert_eq!(
            q[0].claimed_by.as_deref(),
            Some("tab-bld"),
            "ownership = the claimer tab-id"
        );

        // A restricted task doesn't block a lower-priority OPEN task for a
        // mismatched role: selection skips what the role can't take.
        let mut q2 = vec![
            TaskEntry {
                priority: 9,
                ..task_to("hi-restricted", "builder")
            },
            task("lo-open", 0, TaskState::Queued),
        ];
        let got = claim_next(&mut q2, NOW, "tab-rev", "reviewer", LEASE).expect("reviewer claims the open one");
        assert_eq!(
            got.id, "lo-open",
            "the high-prio restricted task is skipped; the open one is claimed"
        );
    }

    // S3 GARDE-FOU (tichef): capacity (role → gates the CLAIM) ≠ ownership
    // (tab-id → gates beat/done). Two peers of the SAME role may both claim, but
    // once one wins, ONLY its tab-id may beat/done; the other (same role, other
    // tab-id) is refused. Capacity and ownership must not be conflated.
    #[test]
    fn capacity_is_not_ownership_same_role_different_tab_id() {
        let mut q = vec![task_to("t1", "builder")];
        // Both peers ARE builders → both are CAPABLE of claiming (capacity).
        assert!(
            role_matches(q[0].to.as_deref(), "builder"),
            "peer-A (builder) may claim"
        );
        assert!(
            role_matches(q[0].to.as_deref(), "builder"),
            "peer-B (builder) may claim"
        );
        // Peer-A wins the claim (serialized); ownership stamps A's TAB-ID, not the role.
        let got = claim_next(&mut q, NOW, "tab-A", "builder", LEASE).expect("A claims");
        assert_eq!(got.id, "t1");
        assert_eq!(
            q[0].claimed_by.as_deref(),
            Some("tab-A"),
            "ownership = tab-A, not \"builder\""
        );
        // Peer-B — SAME role, DIFFERENT tab-id — can't beat or done A's task.
        assert!(
            !beat_task(&mut q, "t1", "tab-B", NOW, LEASE),
            "same role but not the owner → beat refused"
        );
        assert_eq!(
            done_task(&mut q, "t1", "tab-B", NOW),
            DoneOutcome::Stale,
            "same role but not the owner → done is stale (409)"
        );
        // Only the owner tab-id can beat and done it.
        assert!(beat_task(&mut q, "t1", "tab-A", NOW, LEASE), "the owner tab-id beats");
        assert_eq!(
            done_task(&mut q, "t1", "tab-A", NOW),
            DoneOutcome::Completed,
            "the owner tab-id completes"
        );
    }

    // Bonus characterization (like the aligator net): push N with mixed
    // priorities → claims drain by priority DESC then FIFO; file bounded after
    // compaction; roundtrip through parse/encode is byte-faithful.
    #[test]
    fn push_claim_done_roundtrips_and_stays_bounded() {
        let mut q = vec![
            TaskEntry {
                ts: 10,
                ..task("low-old", 0, TaskState::Queued)
            },
            TaskEntry {
                ts: 20,
                ..task("hi-new", 5, TaskState::Queued)
            },
            TaskEntry {
                ts: 5,
                ..task("hi-old", 5, TaskState::Queued)
            },
            TaskEntry {
                ts: 30,
                ..task("low-new", 0, TaskState::Queued)
            },
        ];
        // Priority 5 group drains first, oldest-ts first within it; then priority 0.
        let order: Vec<String> =
            std::iter::from_fn(|| claim_next(&mut q, NOW, "peer-a", "", LEASE).map(|t| t.id)).collect();
        assert_eq!(
            order,
            vec!["hi-old", "hi-new", "low-old", "low-new"],
            "priority DESC then FIFO"
        );
        // Complete two (the owner still holds a live lease), compact → only the
        // still-claimed tail survives (bounded).
        assert_eq!(done_task(&mut q, "hi-old", "peer-a", NOW), DoneOutcome::Completed);
        assert_eq!(done_task(&mut q, "hi-new", "peer-a", NOW), DoneOutcome::Completed);
        let kept = compact_tasks(&q);
        let kept_ids: Vec<&str> = kept.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["low-old", "low-new"], "done pruned, rest kept in order");
        assert!(
            kept.iter().all(|e| e.state == TaskState::Claimed),
            "survivors are claimed, not re-queued"
        );
        // parse ∘ encode is the identity on a line.
        let e = &q[0];
        assert_eq!(parse_tasks(&encode_task_line(e)), vec![e.clone()], "jsonl roundtrip");
    }

    // S1.1: the claim window COMPACTS (aligator parity) → the file stays BOUNDED:
    // accumulated `done` entries are pruned at each claim, the newly-claimed task
    // survives, and a pruned `done` never re-appears (exactly-once).
    #[test]
    fn claim_compacts_done_so_the_file_stays_bounded() {
        let entries = vec![
            task("d1", 0, TaskState::Done), // stale done → must be pruned
            task("q1", 0, TaskState::Queued),
            task("d2", 0, TaskState::Done), // stale done → must be pruned
        ];
        let (claimed, kept, changed) = claim_and_compact(entries, NOW, "peer-a", "", LEASE);
        assert!(changed, "a claim + a prune → a write is needed");
        assert_eq!(
            claimed.as_ref().map(|t| t.id.as_str()),
            Some("q1"),
            "the queued task is claimed"
        );
        // The two done entries are gone; only the now-claimed q1 survives → bounded.
        let kept_ids: Vec<&str> = kept.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["q1"], "done entries pruned at the claim window");
        assert!(
            kept.iter().all(|e| e.state != TaskState::Done),
            "no done entry persists"
        );
        assert_eq!(
            kept[0].state,
            TaskState::Claimed,
            "the claimed task survives as claimed"
        );
        // Claiming again on the bounded file finds nothing (q1 already claimed),
        // and compaction with all-done-pruned already happened → still bounded.
        let (again, kept2, changed2) = claim_and_compact(kept, NOW, "peer-a", "", LEASE);
        assert!(again.is_none(), "nothing left to claim");
        assert!(!changed2, "no claim + nothing to prune → no write");
        assert_eq!(kept2.len(), 1, "file stays bounded (the single claimed task)");
    }

    // S1.2 (Olympe catch): a claim whose PERSIST fails is NOT won — exactly-once
    // must not break silently under an fs fault. If the write fails, the task
    // stays claimable (queued on disk), and a later claim wins it.
    #[test]
    fn claim_not_won_if_persist_fails_so_task_stays_claimable() {
        let entries = vec![task("q1", 0, TaskState::Queued)];
        // Persist FAILS (fs fault) → the claim returns empty, NOT the task.
        let got = perform_claim(entries.clone(), NOW, "peer-a", "", LEASE, |_| {
            Err(std::io::Error::other("disk full"))
        });
        assert!(
            got.is_none(),
            "a claim whose persist fails is not won — empty, never the task"
        );
        // The caller's source is untouched (perform_claim took a copy), so on a
        // real re-read the task is still `queued` → still claimable. Prove the
        // retry: a SUCCEEDING persist now wins the same task.
        let mut persisted: Option<Vec<TaskEntry>> = None;
        let got2 = perform_claim(entries, NOW, "peer-a", "", LEASE, |kept| {
            persisted = Some(kept.to_vec());
            Ok(())
        });
        assert_eq!(
            got2.map(|t| t.id),
            Some("q1".into()),
            "the task stays claimable — a later claim wins it"
        );
        // And the durably-persisted state has it CLAIMED (the win is recorded).
        let saved = persisted.expect("persist ran on the successful retry");
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0].state,
            TaskState::Claimed,
            "the won claim is what gets persisted"
        );
    }

    #[test]
    fn queue_path_sanitises_traversal() {
        // A hostile queue name can't escape the tasks dir.
        let p = queue_path("../../etc/passwd");
        assert!(
            p.file_name().unwrap().to_string_lossy().starts_with("______etc_passwd"),
            "{p:?}"
        );
        assert!(p.ends_with("________etc_passwd.jsonl") || p.to_string_lossy().contains("tasks"));
    }

    #[test]
    fn parse_tasks_skips_blank_and_garbage() {
        let body = "\n  \nnot json\n{\"ts\":1,\"id\":\"a\",\"queue\":\"q\",\"payload\":\"p\",\"state\":\"queued\"}\n";
        let ts = parse_tasks(body);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].id, "a");
        assert_eq!(ts[0].priority, 0, "priority defaults to 0 when absent");
        assert_eq!(ts[0].state, TaskState::Queued);
    }
}
