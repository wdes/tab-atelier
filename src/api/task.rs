// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `task` primitive (#11) route handlers: push / claim / beat / done / list.
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
    queue_path, queue_view, write_tasks_atomic,
};

use super::{TabSnapshot, error_json, respond_json};

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

/// A non-empty string field from the JSON body, else `None`.
fn body_str(body: &[u8], key: &str) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get(key).and_then(|c| c.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())
}

/// The caller's ROLE for the S3 capacity gate: the body `role` override
/// (`--as <role>`) if given, else the role read off the caller's CARD — the
/// assignment role (`role_of`) or, failing that, the specialty. Resolved from the
/// snapshot by the caller's tab-id (`claimed_by`). Empty when the tab has no card
/// (then it can only claim un-restricted tasks). Capacity ≠ ownership: this never
/// touches `claimed_by`.
fn resolve_role(snap: &TabSnapshot, tab_id: &str, body: &[u8]) -> String {
    if let Some(r) = body_str(body, "role") {
        return r;
    }
    let Some(tab) = snap.tabs.iter().find(|t| &*t.id == tab_id) else {
        return String::new();
    };
    let role = super::role_of(tab.assignment.as_deref());
    if role.is_empty() {
        tab.specialty.as_deref().unwrap_or_default().to_string()
    } else {
        role
    }
}

/// `POST /task/{queue}/push` — enqueue `{payload, priority?}`. Returns 201 {id}.
pub(super) fn push<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str, body_bytes: &[u8]) {
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
        // S3 — an optional `to` role requirement; absent/empty = claimable by all.
        to: body_str(body_bytes, "to"),
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
pub(super) fn claim<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str, body_bytes: &[u8]) {
    let path = queue_path(queue);
    let now = crate::unix_millis();
    let claimer = body_claimer(body_bytes);
    let lease_ms = body_lease_ms(body_bytes);
    // The whole read-modify-write runs under the snapshot mutex → the CAS. A
    // claim is WON only if the state change is DURABLY persisted (perform_claim):
    // a failed write returns None → 204, the on-disk task stays claimable, so a
    // persist fault can't silently double-claim.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // S3 capacity gate: resolve the caller's ROLE (body override or its card) — a
    // `--to <role>` task is only claimable by a matching role. Ownership stays on
    // `claimer` (the tab-id); role never touches it.
    let caller_role = resolve_role(&guard, &claimer, body_bytes);
    let entries = parse_tasks(&std::fs::read_to_string(&path).unwrap_or_default());
    let claimed = perform_claim(entries, now, &claimer, &caller_role, lease_ms, |kept| {
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
pub(super) fn beat<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, id: &str, body_bytes: &[u8]) {
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
pub(super) fn done<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, id: &str, body_bytes: &[u8]) {
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

/// `GET /task/{queue}/list` — the queue's READ-ONLY read-model (S4): each task's
/// current state (`queued` / `claimed@<peer>` / `done`), an expired-lease claim
/// folded to `queued` (reclaimable) at read time. Mutates NOTHING; a missing
/// queue file reads as an empty list. Returns 200 with `{queue, tasks:[…]}`.
pub(super) fn list<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, queue: &str) {
    let now = crate::unix_millis();
    // Read under the snapshot lock so the read-model can't observe a torn write.
    // ponytail: the atomic tmp+rename already prevents torn reads, so this lock is
    // stricter than strictly needed — kept for uniformity with the write paths;
    // drop it if `list` ever contends the daemon's hot path.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let entries = parse_tasks(&std::fs::read_to_string(queue_path(queue)).unwrap_or_default());
    let view = queue_view(queue, &entries, now);
    drop(guard);
    respond_json(stream, 200, &serde_json::to_string(&view).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status code from a buffered HTTP/1.1 response the handlers write.
    fn status_of(resp: &[u8]) -> u16 {
        let head = String::from_utf8_lossy(resp);
        head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    /// The JSON body (after the blank-line header terminator) of a response.
    fn body_of(resp: &[u8]) -> String {
        let s = String::from_utf8_lossy(resp);
        s.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()
    }

    /// A snapshot + a queue dir isolated on a fresh tmpdir (via the
    /// `TAB_ATELIER_TASKS_DIR` seam), so the real handler roundtrip touches disk
    /// without escaping to the shared XDG state. The dir string is returned to
    /// keep it alive for the test's lifetime.
    fn harness() -> (Arc<Mutex<TabSnapshot>>, String) {
        // A unique dir per test process/thread — cargo runs tests in threads, so
        // key it on the thread name to avoid cross-test queue collisions.
        let base = std::env::temp_dir().join(format!(
            "ta-task-it-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace(&[':', ' '][..], "_")
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("tmp tasks dir");
        // Point THIS thread's tasks_dir at the tmpdir (no unsafe env::set_var).
        crate::cli::task::set_tasks_dir_for_test(base.clone());
        (
            Arc::new(Mutex::new(crate::api::test_snapshot(vec![]))),
            base.to_string_lossy().to_string(),
        )
    }

    // The anti-built≠wired proof, through the REAL route handlers (push then
    // claim, body-parse → snapshot lock → perform_claim → atomic disk write →
    // HTTP response). Two claims serialize under the shared mutex → EXACTLY ONE
    // wins (200 with the payload); the other gets 204. Not a mock: the roundtrip
    // hits disk and the same lock the daemon serializes writes behind.
    #[test]
    fn two_claims_exactly_one_wins_via_the_api() {
        let (state, _dir) = harness();

        let mut resp = Vec::new();
        push(&mut resp, &state, "q", br#"{"payload":"do-it"}"#);
        assert_eq!(status_of(&resp), 201, "push responds 201");
        assert!(body_of(&resp).contains("\"id\""), "push returns an id");

        // First claim WINS the single task.
        let mut a = Vec::new();
        claim(&mut a, &state, "q", br#"{"claimed_by":"peer-a"}"#);
        assert_eq!(status_of(&a), 200, "the first claim wins");
        assert!(body_of(&a).contains("do-it"), "the winner receives the payload");

        // Second claim, live lease still held → nothing claimable → 204.
        let mut b = Vec::new();
        claim(&mut b, &state, "q", br#"{"claimed_by":"peer-b"}"#);
        assert_eq!(status_of(&b), 204, "the second claim gets nothing — never both");
    }

    // The TTL half of the proof, through the REAL handlers: a claim with a
    // zero-second lease expires immediately, so a later claim (a wall-clock
    // moment on) RE-POOLS the orphan and wins it. `now` is `unix_millis()` (real
    // clock), so a tiny sleep guarantees the second claim sees `lease_until < now`
    // — the calibration a millisecond-resolution clock needs.
    #[test]
    fn expired_lease_repools_via_the_api() {
        let (state, _dir) = harness();

        let mut resp = Vec::new();
        push(&mut resp, &state, "q", br#"{"payload":"orphan-me"}"#);
        assert_eq!(status_of(&resp), 201);

        // Claim with a 0s lease → lease_until == now → expires at the next tick.
        let mut a = Vec::new();
        claim(&mut a, &state, "q", br#"{"claimed_by":"peer-a","lease_secs":0}"#);
        assert_eq!(status_of(&a), 200, "peer-a claims it");

        // Advance the wall clock past the (zero) lease so the orphan is reclaimable.
        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut b = Vec::new();
        claim(&mut b, &state, "q", br#"{"claimed_by":"peer-b"}"#);
        assert_eq!(status_of(&b), 200, "the expired lease re-pools → peer-b reclaims it");
        assert!(
            body_of(&b).contains("orphan-me"),
            "the reclaimed task carries its payload"
        );
    }
}
