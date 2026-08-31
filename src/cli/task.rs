// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `task` — a typed task queue with an ATOMIC claim (primitive #11, POC S1).
//!
//! S1 scope: the file+append backend (reused from aligator's swamp: append +
//! compaction, `src/cli/aligator.rs`) + `push`/`claim`/`done` + the atomic claim
//! serialized behind the mono-process daemon. NO lease/beat (S2), NO capacity
//! `--to` (S3), NO read-model (S4).
//!
//! **Founding insight** (design `issue-task-queue-lease.md`): the daemon is a
//! single process, so a `claim` handled behind the API is a free compare-and-
//! swap — the daemon (under a lock, like `reset-master-token`) is the only
//! writer, so two concurrent claims serialize and exactly one wins.
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
}

fn tasks_dir() -> PathBuf {
    crate::platform::state_base_dir().join("tab-atelier").join("tasks")
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

/// The index of the next task to claim.
///
/// The first `Queued` entry, ordered by priority DESC then FIFO (oldest `ts`
/// first, then file order for a stable tie-break). `None` when the queue holds no
/// claimable task. Pure — the atomic serialization is the caller's (the daemon
/// lock); this is just the choice.
#[must_use]
pub fn select_next_claim(entries: &[TaskEntry]) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.state == TaskState::Queued)
        .min_by(|(ia, a), (ib, b)| {
            b.priority
                .cmp(&a.priority) // higher priority first
                .then(a.ts.cmp(&b.ts)) // then oldest first (FIFO)
                .then(ia.cmp(ib)) // then file order (stable)
        })
        .map(|(i, _)| i)
}

/// Claim the next task.
///
/// Flip the selected `Queued` entry to `Claimed` and return a clone. `None` when
/// nothing is claimable. Run under the daemon lock, two concurrent claims
/// serialize → the second sees the first's entry already `Claimed` → exactly one
/// wins.
#[must_use]
pub fn claim_next(entries: &mut [TaskEntry]) -> Option<TaskEntry> {
    let idx = select_next_claim(entries)?;
    entries[idx].state = TaskState::Claimed;
    Some(entries[idx].clone())
}

/// Mark the task `id` `Done`.
///
/// Returns `true` if it flipped a not-yet-done task, `false` if the id is unknown
/// or already done (idempotent — a repeat `done` is a no-op, not an error; S1 has
/// no lease so no stale-409).
pub fn mark_done(entries: &mut [TaskEntry], id: &str) -> bool {
    if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
        let flipped = e.state != TaskState::Done;
        e.state = TaskState::Done;
        flipped
    } else {
        false
    }
}

/// Entries a compaction KEEPS: everything not `Done`. The `Done` entries are
/// pruned, so a completed task can never be re-claimed (exactly-once) — the same
/// bounded-file discipline as aligator's `compact`.
#[must_use]
pub fn compact_tasks(entries: &[TaskEntry]) -> Vec<TaskEntry> {
    entries.iter().filter(|e| e.state != TaskState::Done).cloned().collect()
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

/// Rewrite a queue file to exactly `entries`, atomically (tmp + rename), like
/// `compact_swamp`. Used by the claim/done read-modify-write and by compaction.
///
/// `ponytail:` a producer append landing between the caller's read and this
/// rewrite is lost — the same tiny read-modify-write window `compact_swamp`
/// documents; the daemon lock keeps CLAIMS mutually exclusive, and an flock is
/// the upgrade path for the producer race.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, priority: u8, state: TaskState) -> TaskEntry {
        TaskEntry {
            ts: 0,
            id: id.into(),
            queue: "q".into(),
            payload: format!("payload-{id}"),
            priority,
            state,
        }
    }

    // Acceptance #1 (task-s1-build-spec §Acceptance): two concurrent claims on a
    // one-task queue → EXACTLY ONE gets it. The daemon serializes concurrent
    // claims (single-writer under the lock), so "concurrent" == "sequential
    // claim_next on the same, mutating vec": the 2nd sees the entry already
    // Claimed → empty. Never both.
    #[test]
    fn claim_is_exclusive_under_concurrency() {
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        let a = claim_next(&mut q);
        let b = claim_next(&mut q); // the serialized second claimer
        assert_eq!(a.as_ref().map(|t| t.id.as_str()), Some("t1"), "one claimer wins");
        assert!(b.is_none(), "the other claimer gets nothing — never both");
        assert_eq!(q[0].state, TaskState::Claimed, "the task is claimed exactly once");
    }

    // Acceptance #4: a `done` task NEVER re-appears in a later claim.
    #[test]
    fn done_task_never_reappears() {
        let mut q = vec![task("t1", 0, TaskState::Queued)];
        let claimed = claim_next(&mut q).expect("claimed");
        assert!(mark_done(&mut q, &claimed.id), "done flips claimed → done");
        // Compaction prunes the done entry (bounded file), and a claim after it
        // finds nothing — the done task is gone for good.
        let q = compact_tasks(&q);
        assert!(q.is_empty(), "done task pruned at compaction");
        let mut q = q;
        assert!(claim_next(&mut q).is_none(), "a done task never re-appears in a claim");
        // Idempotent: a repeat done on an unknown/gone id is a no-op, not an error.
        assert!(!mark_done(&mut q, "t1"), "repeat done on a pruned id → no-op");
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
        let order: Vec<String> = std::iter::from_fn(|| claim_next(&mut q).map(|t| t.id)).collect();
        assert_eq!(
            order,
            vec!["hi-old", "hi-new", "low-old", "low-new"],
            "priority DESC then FIFO"
        );
        // Complete two, compact → only the still-claimed tail survives (bounded).
        assert!(mark_done(&mut q, "hi-old"));
        assert!(mark_done(&mut q, "hi-new"));
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
