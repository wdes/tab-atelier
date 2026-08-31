// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `task` primitive (#11) S1 route handlers: push / claim / done.
//!
//! The ATOMIC part: `claim` (and `done`) do their file read-modify-write while
//! holding the shared snapshot mutex — the daemon's single-writer serialization
//! point (the same one `reset-master-token` hot-swaps under). Two concurrent
//! claims therefore serialize → the second sees the first's entry already
//! `Claimed` → exactly one wins. No lock invention: we exploit the mono-process.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::cli::task::{TaskEntry, TaskState, mark_done, parse_tasks, queue_files, queue_path, write_tasks_atomic};

use super::super::{TabSnapshot, error_json, respond_json};

/// `POST /task/{queue}/push` — enqueue `{payload, priority?}`. Returns 201 {id}.
pub(in crate::api) fn push<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str, body_bytes: &[u8]) {
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(payload) = parsed.get("payload").and_then(|v| v.as_str()) else {
        error_json(stream, 400, "missing `payload` field");
        return;
    };
    let priority = parsed
        .get("priority")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(0);
    let entry = TaskEntry {
        ts: crate::unix_millis() / 1000,
        id: crate::default_tab_id(),
        queue: queue.to_string(),
        payload: payload.to_string(),
        priority,
        state: TaskState::Queued,
    };
    // Serialize the append under the snapshot lock too, so a push racing a
    // compacting claim can't be dropped by the claim's rewrite window.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let res = crate::cli::task::append_task_line(&queue_path(queue), &entry);
    drop(guard);
    match res {
        Ok(()) => respond_json(stream, 201, &format!(r#"{{"id":"{}"}}"#, entry.id)),
        Err(e) => error_json(stream, 500, &format!("enqueue: {e}")),
    }
}

/// `POST /task/{queue}/claim` — atomic claim of the next task. 200 {id,payload}
/// when one is claimed, 204 (empty) when the queue holds nothing claimable.
pub(in crate::api) fn claim<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str) {
    let path = queue_path(queue);
    // The whole read-modify-write runs under the snapshot mutex → the CAS. A
    // claim is WON only if the state change is DURABLY persisted: perform_claim
    // hands the task out ONLY when the write succeeds. If the write fails (fs
    // fault), it returns None → 204, the file still shows the task queued → it
    // stays claimable, so a persist failure can't silently double-claim.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
    let claimed = crate::cli::task::perform_claim(entries, |kept| {
        write_tasks_atomic(&path, kept).inspect_err(|e| {
            log::error!("task claim: persist failed for queue {queue}: {e} — not handing out the task");
        })
    });
    drop(guard);
    match claimed {
        Some(t) => {
            let body =
                serde_json::to_string(&serde_json::json!({ "id": t.id, "payload": t.payload })).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        // 204 No Content: the queue is empty / everything already claimed.
        None => respond_json(stream, 204, ""),
    }
}

/// `POST /task/{id}/done` — complete a task by id (idempotent). Scans the queue
/// files to find the id's queue (S1 addresses by id alone). Always 200.
///
/// S2 NOTE (not built now): `done` scans ALL queue files while holding the main
/// snapshot lock — fine at S1 scale, but under contention (many queues, or S2's
/// frequent heartbeats also taking the lock) a DEDICATED task mutex would keep
/// this off the daemon's hot path. Deferred to S2 with lease/beat.
pub(in crate::api) fn done<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, id: &str) {
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut flipped = false;
    for path in queue_files() {
        let mut entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
        if mark_done(&mut entries, id) {
            let _ = write_tasks_atomic(&path, &entries);
            flipped = true;
            break;
        }
    }
    drop(guard);
    // Idempotent: unknown / already-done id → still 200 (exactly-once, no
    // stale-409 in S1 — that's S2's lease). `flipped` just says whether this
    // call did the transition.
    respond_json(stream, 200, &format!(r#"{{"done":"{id}","flipped":{flipped}}}"#));
}
