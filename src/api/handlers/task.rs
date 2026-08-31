// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `task` primitive (#11) route handlers: push / claim / beat / done.
//!
//! The ATOMIC part: claim / beat / done do their file read-modify-write while
//! holding the shared snapshot mutex — the daemon's single-writer serialization
//! point (the same one `reset-master-token` hot-swaps under). Two concurrent
//! claims therefore serialize → the second sees the first's entry already
//! `Claimed` (with a live lease) → exactly one wins. No lock invention: we
//! exploit the mono-process.
//!
//! S2 adds lease/beat/orphan-reclaim (generalising triage-tickets' heartbeat/
//! TTL): a claim stamps `claimed_by` + `lease_until`; an expired lease frees the
//! task for another idle peer; `beat` renews; `done` is honoured only while the
//! caller still holds a valid lease (else 409 stale).
//!
//! Contention note: claim/beat/done run under the MAIN snapshot lock, and `done`
//! scans the (bounded, S1.1) queue files under it. At S1/S2 scale that scan is
//! cheap, so it's kept simple; a dedicated task mutex is the upgrade path only if
//! the daemon's hot path ever measurably contends here.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::cli::task::{
    DEFAULT_LEASE_MS, DoneOutcome, TaskEntry, TaskState, beat_task, done_task, parse_tasks, perform_claim, queue_files,
    queue_path, write_tasks_atomic,
};

use super::super::{TabSnapshot, error_json, respond_json};

/// `claimed_by` from the request body, defaulting to `"anon"` (S3 will map it to
/// the caller's card role; S2 just needs a stable owner identity for beat/done).
fn body_claimer(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("claimed_by").and_then(|c| c.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anon".to_string())
}

/// `lease_secs` from the body → millis, defaulting to [`DEFAULT_LEASE_MS`].
fn body_lease_ms(body: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("lease_secs").and_then(serde_json::Value::as_u64))
        .map_or(DEFAULT_LEASE_MS, |s| s.saturating_mul(1000))
}

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
        claimed_by: None,
        lease_until: None,
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

/// `POST /task/{queue}/claim` — atomic claim of the next claimable task (queued
/// OR an expired-lease orphan). Body `{claimed_by, lease_secs?}`. 200
/// {`id,payload,lease_until`} on a win, 204 (empty) when nothing is claimable.
pub(in crate::api) fn claim<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str, body_bytes: &[u8]) {
    let path = queue_path(queue);
    let now = crate::unix_millis();
    let claimer = body_claimer(body_bytes);
    let lease_ms = body_lease_ms(body_bytes);
    // The whole read-modify-write runs under the snapshot mutex → the CAS. A
    // claim is WON only if the state change is DURABLY persisted (perform_claim):
    // a failed write returns None → 204, the on-disk task stays claimable, so a
    // persist fault can't silently double-claim.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
    let claimed = perform_claim(entries, now, &claimer, lease_ms, |kept| {
        write_tasks_atomic(&path, kept).inspect_err(|e| {
            log::error!("task claim: persist failed for queue {queue}: {e} — not handing out the task");
        })
    });
    drop(guard);
    match claimed {
        Some(t) => {
            let body = serde_json::to_string(&serde_json::json!({
                "id": t.id,
                "payload": t.payload,
                "lease_until": t.lease_until,
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        None => respond_json(stream, 204, ""),
    }
}

/// `POST /task/{id}/beat` — renew the lease. Body `{claimed_by, lease_secs?}`.
/// 200 {`lease_until`} when the caller is the current claimer; 409 otherwise
/// (unknown / not claimed / already reclaimed by someone else).
pub(in crate::api) fn beat<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, id: &str, body_bytes: &[u8]) {
    let now = crate::unix_millis();
    let claimer = body_claimer(body_bytes);
    let lease_ms = body_lease_ms(body_bytes);
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut renewed = None;
    for path in queue_files() {
        let mut entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
        if beat_task(&mut entries, id, &claimer, now, lease_ms) {
            let _ = write_tasks_atomic(&path, &entries);
            renewed = entries.iter().find(|e| e.id == id).and_then(|e| e.lease_until);
            break;
        }
    }
    drop(guard);
    match renewed {
        Some(lease_until) => respond_json(stream, 200, &format!(r#"{{"lease_until":{lease_until}}}"#)),
        None => error_json(stream, 409, "not the current claimer (lease lost or task gone)"),
    }
}

/// `POST /task/{id}/done` — complete a task by id. Body `{claimed_by}`. 200 when
/// the caller still holds a valid lease (or the task was already done by it);
/// 409 STALE when the lease expired / the task was re-claimed / wrong claimer.
pub(in crate::api) fn done<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, id: &str, body_bytes: &[u8]) {
    let now = crate::unix_millis();
    let claimer = body_claimer(body_bytes);
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut outcome = DoneOutcome::NotFound;
    for path in queue_files() {
        let mut entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
        let o = done_task(&mut entries, id, &claimer, now);
        if o != DoneOutcome::NotFound {
            if o == DoneOutcome::Completed {
                let _ = write_tasks_atomic(&path, &entries);
            }
            outcome = o;
            break;
        }
    }
    drop(guard);
    match outcome {
        // Completed / AlreadyDone / NotFound → 200 (idempotent; a done for a
        // never-seen id has nothing to protect).
        DoneOutcome::Completed | DoneOutcome::AlreadyDone | DoneOutcome::NotFound => {
            respond_json(stream, 200, &format!(r#"{{"done":"{id}"}}"#));
        }
        // Stale: the lease expired / the task was re-claimed / wrong claimer.
        DoneOutcome::Stale => error_json(stream, 409, "stale done — lease expired or task re-claimed"),
    }
}
