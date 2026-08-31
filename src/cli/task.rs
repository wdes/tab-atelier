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
pub fn claim_and_compact(mut entries: Vec<TaskEntry>) -> (Option<TaskEntry>, Vec<TaskEntry>, bool) {
    let before = entries.len();
    let claimed = claim_next(&mut entries);
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

/// `tab-atelier task <push|claim|done> …` — the producer/consumer CLI.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("push") => run_push(&args[1..]),
        Some("claim") => run_claim(&args[1..]),
        Some("done") => run_done(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  tab-atelier task push --queue <q> [--priority N] \"<payload>\"\n  \
                 tab-atelier task claim --queue <q>\n  tab-atelier task done <task-id>"
            );
            2
        }
    }
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
    let body = serde_json::json!({ "payload": payload, "priority": priority }).to_string();
    match post_task(&format!("{queue}/push"), &body) {
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
    match post_task(&format!("{queue}/claim"), "") {
        // 200 → a task; print {id,payload}. 204 → empty queue; print nothing.
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

fn run_done(args: &[String]) -> i32 {
    let Some(id) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("task done: a <task-id> is required");
        return 2;
    };
    match post_task(&format!("{id}/done"), "") {
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
        let (claimed, kept, changed) = claim_and_compact(entries);
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
        let (again, kept2, changed2) = claim_and_compact(kept);
        assert!(again.is_none(), "nothing left to claim");
        assert!(!changed2, "no claim + nothing to prune → no write");
        assert_eq!(kept2.len(), 1, "file stays bounded (the single claimed task)");
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
