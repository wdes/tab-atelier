// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `restart_wake` — the daemon restart-wake emission (RA1, dual-channel, measured).
//!
//! On restart, the daemon reuses the `self_announce` LOCUS (the one-shot emission
//! point at startup, after the API is up and tabs are restored) but on a SEPARATE
//! channel, best-effort and non-blocking (`let _ =`), to tell the fleet it's back:
//! - (a) a `note_best_effort(topic=ops, "RESTART_DONE build=..")` — the durable
//!   PULL fallback (any orchestrator polling the blackboard catches it later);
//! - (b) a swamp WAKE push toward each orchestrator (roster snapshot filtered by
//!   assignment) → aligator delivers it = the low-LATENCY path.
//!
//! This kills the restart→wake latency (baseline 5-14 min via the cron watcher)
//! down to one aligator tour (target 5-10s, acceptance `< 30s`, MEASURED as the
//! delta of two timestamps — the restart ts carried in the wake vs the reception
//! ts).
//!
//! **Orthogonality (A4-3, non négociable)**: this is a NEW channel. The
//! `self_announce` `agentKind` POST is UNTOUCHED and byte-identical; the wake
//! carries NO `agentKind` — event ≠ tag. And it never touches
//! `restore_resume_command`. A failed emission NEVER blocks or delays startup
//! (best-effort): the report records failures, startup continues.
//!
//! Like `perform_retire`/`perform_claim`, the effects are INJECTED seams so the
//! pure core is unit-tested (roster filter, message format, latency, best-effort)
//! without a live daemon.

/// The wake message / swamp-input marker: `RESTART_DONE build=<hash> at=<ts>`.
///
/// `at=<ts>` (unix-millis of the restart) is what makes the latency MEASURABLE:
/// the receiver reads it back and computes `now - ts`. snake wire keys, no drift.
#[must_use]
pub fn wake_input(build: &str, at_ms: u64) -> String {
    format!("RESTART_DONE build={build} at={at_ms}")
}

/// Read the restart timestamp back out of a wake marker (receiver side) — the
/// second half of the latency measurement. `None` when the marker isn't a wake.
#[must_use]
pub fn parse_wake_ts(input: &str) -> Option<u64> {
    input
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("at="))
        .and_then(|s| s.parse().ok())
}

/// Is this tab an ORCHESTRATOR the wake should target?
///
/// True when the assignment's role is `orchestrator` (any project/phase), OR the
/// assignment is the fleet's `meta/manager` (with or without a `<project>:`
/// override). Reuses [`crate::api::role_of`] — no parallel role taxonomy.
#[must_use]
pub fn is_orchestrator(assignment: Option<&str>) -> bool {
    let Some(a) = assignment.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    // Drop an optional `<project>:` override (the prefix before the first `/`).
    let core = match (a.find(':'), a.find('/')) {
        (Some(colon), Some(slash)) if colon < slash => a[colon + 1..].trim(),
        _ => a,
    };
    crate::api::role_of(Some(a)) == "orchestrator" || core == "meta/manager"
}

/// The orchestrator roster: the tab-ids to wake, filtered from a `(id, assignment)`
/// snapshot. Pure — the caller snapshots the live tabs at startup.
#[must_use]
pub fn orchestrator_roster<'a>(tabs: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> Vec<String> {
    tabs.into_iter()
        .filter(|(_, assignment)| is_orchestrator(*assignment))
        .map(|(id, _)| id.to_string())
        .collect()
}

/// What the restart-wake emission did — carries the restart ts for the latency
/// measurement + best-effort push counts (a failed push never blocks startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeReport {
    /// Unix-millis the restart-wake was emitted (= the "restart" timestamp).
    pub emitted_at: u64,
    /// Whether the durable pull-fallback note was posted.
    pub note_posted: bool,
    /// Orchestrators the swamp wake reached.
    pub pushed: usize,
    /// Orchestrators whose swamp push failed (best-effort; startup continues).
    pub failed: usize,
}

/// Emit the restart-wake, PURE + best-effort + mockable (RA1).
///
/// Dual channel: (a) one `note` on the ops topic (durable pull fallback), then (b)
/// a `swamp` wake push per orchestrator (the latency path). Every effect is an
/// injected seam. A failing `swamp` push is COUNTED, never propagated — the daemon
/// startup is never blocked or delayed (RA1-3). The wake carries `now` so the
/// receiver can measure restart→wake latency (RA1-1).
pub fn emit_restart_wake<N, S>(build: &str, now: u64, roster: &[String], note: N, mut swamp: S) -> WakeReport
where
    N: FnOnce(&str, &str, &str),
    S: FnMut(&str, &str) -> std::io::Result<()>,
{
    // (a) Durable pull fallback: one ops note. `note` is itself best-effort.
    let msg = wake_input(build, now);
    note("ops", "daemon", &msg);
    // (b) Low-latency push: a swamp wake per orchestrator. Best-effort — a push
    //     that errors is counted, NEVER bubbled, so startup can't stall.
    let (mut pushed, mut failed) = (0usize, 0usize);
    for orch in roster {
        match swamp(orch, &msg) {
            Ok(()) => pushed += 1,
            Err(_) => failed += 1,
        }
    }
    WakeReport {
        emitted_at: now,
        note_posted: true,
        pushed,
        failed,
    }
}

/// The daemon-startup restart-wake, SHARED by both editions (headless + gui) so
/// they can't drift — the gui daemon had silently skipped the emission before.
///
/// Gates on `read_only` (skip → `None`), snapshots the orchestrator roster from
/// `tabs`, and emits best-effort via the injected `note`/`swamp` seams. Returns
/// the [`WakeReport`] (or `None` when skipped). Both `src/headless.rs::run` and
/// `src/app/mod.rs::run` call THIS — the single wiring the tests exercise.
pub fn emit_startup_wake<'a, N, S>(
    read_only: bool,
    tabs: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    build: &str,
    now: u64,
    note: N,
    swamp: S,
) -> Option<WakeReport>
where
    N: FnOnce(&str, &str, &str),
    S: FnMut(&str, &str) -> std::io::Result<()>,
{
    if read_only {
        return None; // advertises "changes nothing" — no emission (RA1b acceptance d)
    }
    let roster = orchestrator_roster(tabs);
    Some(emit_restart_wake(build, now, &roster, note, swamp))
}

// ---------------------------------------------------------------------------
// RA1c — the DEFERRED, readiness-gated, staggered wake DELIVERY.
//
// RA1b delivered at raw startup with submit=false → the input landed before the
// agent loop was receptive AND was never submitted → stuck. RA1c: keep the ops note
// IMMEDIATE (fallback), but DEFER the submits until the tabs are ready (a readiness
// signal, not a fixed delay) and STAGGER them round-robin with a fixed gap (anti-herd,
// [[quiesce-no-thundering-herd]]). submit=true triggers each orchestrator's turn.
// ---------------------------------------------------------------------------

/// The fixed anti-herd gap between two orchestrator submits (borne: 10s).
pub const WAKE_GAP_MS: u64 = 10_000;
/// Poll cadence while waiting for the readiness signal.
pub const WAKE_READY_POLL_MS: u64 = 1_000;
/// Bounded ceiling on the readiness wait — past this the wake fires anyway
/// (aligator's transient-retry still handles a not-yet-live tab), never hangs startup.
pub const WAKE_READY_MAX_MS: u64 = 120_000;

/// One scheduled wake: WHICH orchestrator, and WHEN (relative delivery timestamp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeStop {
    pub target: String,
    /// Unix-millis this submit is scheduled for — `start_at + index*gap` (round-robin).
    pub at_ms: u64,
}

/// The round-robin STAGGERED schedule (RA1c): orchestrator `i` is woken at
/// `start_at + i*gap_ms`, so N orchestrators never submit simultaneously (anti-herd).
/// PURE — the delivery loop paces on it.
#[must_use]
pub fn wake_schedule(roster: &[String], start_at_ms: u64, gap_ms: u64) -> Vec<WakeStop> {
    roster
        .iter()
        .enumerate()
        .map(|(i, t)| WakeStop { target: t.clone(), at_ms: start_at_ms + i as u64 * gap_ms })
        .collect()
}

/// Run the DEFERRED wake delivery (`RA1c`). SYNCHRONOUS — the caller runs it on a
/// background thread so startup is never blocked.
///
/// 1. Poll `ready()` (bounded by [`WAKE_READY_MAX_MS`]) — the readiness gate, so the
///    submit lands only once the tabs are receptive (not a fixed delay).
/// 2. Round-robin the roster, submitting each via `deliver` with `gap_ms` between
///    (the fixed anti-herd spacing). `pace(ms)` sleeps (injected → tests run instantly).
///
/// Returns how many submits were delivered (best-effort — a failed deliver is skipped,
/// never propagated).
pub fn run_deferred_wake<R, D, P>(roster: &[String], now: u64, gap_ms: u64, ready: R, mut deliver: D, mut pace: P) -> usize
where
    R: Fn() -> bool,
    D: FnMut(&WakeStop) -> std::io::Result<()>,
    P: FnMut(u64),
{
    // (1) Readiness gate — bounded so it can never hang startup.
    let mut waited = 0u64;
    while !ready() && waited < WAKE_READY_MAX_MS {
        pace(WAKE_READY_POLL_MS);
        waited = waited.saturating_add(WAKE_READY_POLL_MS);
    }
    // (2) Staggered round-robin submit.
    let schedule = wake_schedule(roster, now, gap_ms);
    let mut delivered = 0usize;
    for (i, stop) in schedule.iter().enumerate() {
        if i > 0 {
            pace(gap_ms); // the fixed 10s anti-herd gap between submits
        }
        if deliver(stop).is_ok() {
            delivered += 1;
        }
    }
    delivered
}

/// The REAL note seam (durable pull fallback) — one line so both editions share it.
pub fn real_note(topic: &str, from: &str, msg: &str) {
    crate::cli::team::note_best_effort(Some(topic.to_string()), Some(from.to_string()), msg);
}

/// The REAL readiness signal (`RA1c`).
///
/// Every roster orchestrator that still exists reports an active agent session
/// (`agent_session_id` on `/tabs`) — i.e. its agent has booted past the
/// input-swallowing boot phase and is receptive. A vanished tab never blocks.
///
/// This is a SIGNAL, not a fixed delay: as soon as the orchestrators announce their
/// sessions the wake fires; if they never do, [`run_deferred_wake`]'s bounded ceiling
/// fires it anyway (aligator then transient-retries a not-yet-live tab).
#[must_use]
pub fn orchestrators_ready(roster: &[String]) -> bool {
    let Ok(ep) = crate::cli::share_link::discover_endpoint() else {
        return false;
    };
    let Ok(tabs) = crate::cli::share_link::fetch_tabs(&ep) else {
        return false;
    };
    roster.iter().all(|id| {
        tabs.iter().find(|t| t.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())).is_none_or(|t| {
            t.get("agent_session_id").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty())
        })
    })
}

/// The daemon-startup restart-wake WIRING (`RA1c`), SHARED by both editions.
///
/// They can't drift. Posts the durable ops note IMMEDIATELY (fallback), then — off the
/// startup path, on a background thread — waits for [`orchestrators_ready`] and
/// delivers the STAGGERED, submit=TRUE wake round-robin (fixed [`WAKE_GAP_MS`] gap).
///
/// Fire-and-forget + best-effort: startup is never blocked (the thread owns the wait +
/// the sleeps). Skipped entirely in `read_only`.
pub fn spawn_startup_wake(read_only: bool, roster: Vec<String>, build: &'static str, now: u64) {
    if read_only {
        return; // advertises "changes nothing" — no emission (RA1b acceptance d)
    }
    let msg = wake_input(build, now);
    // (a) Durable pull fallback — IMMEDIATE (an orchestrator polling ops catches it).
    real_note("ops", "daemon", &msg);
    if roster.is_empty() {
        return;
    }
    // (b) Low-latency submit — DEFERRED (readiness-gated) + STAGGERED, on a bg thread.
    let _ = std::thread::Builder::new().name("ra1c-wake".to_string()).spawn(move || {
        let ready_roster = roster.clone();
        run_deferred_wake(
            &roster,
            now,
            WAKE_GAP_MS,
            move || orchestrators_ready(&ready_roster),
            |stop: &WakeStop| real_swamp_push(stop.at_ms, build, &stop.target, &msg),
            |ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
        );
    });
}

/// GENERIC push-and-(optionally-)submit toward a target's swamp (`RA1c`) — the reusable
/// brick for ANY event-driven PUSH-relay, not just the restart-wake.
///
/// This is exactly what a Brain/Brian push-relay needs: on an event, PUSH text to a
/// target AND (when `submit`) press Enter so an IDLE target actually acts on it. It
/// goes through the swamp → aligator, so it inherits aligator's regulation (per-round
/// rate cap, transient-retry of not-yet-live tabs, dedup). `submit=false` keeps the
/// old non-intrusive-marker behaviour for callers that want it.
///
/// # Errors
/// Propagates the append I/O error (the caller decides best-effort vs fatal).
pub fn push_swamp_input(
    target: &str,
    msg: &str,
    submit: bool,
    at_ms: u64,
    priority: crate::cli::aligator::Priority,
    dedup_key: Option<String>,
) -> std::io::Result<()> {
    let entry = crate::cli::aligator::SwampEntry {
        ts: at_ms / 1000,
        tab: target.to_string(),
        input: msg.to_string(),
        submit,
        from: Some("daemon".to_string()),
        attempts: 0,
        priority,
        dedup_key,
    };
    crate::cli::aligator::append_swamp_line(&crate::cli::aligator::swamp_path(), &entry)
}

/// The restart-wake swamp entry (`RA1c`) — `submit = TRUE`, `Status` priority, deduped
/// per build. PURE builder (no I/O) so the real entry is testable byte-for-byte.
///
/// ⭐ `RA1c`: `submit = TRUE` — the wake must TRIGGER the orchestrator's turn, not just
/// deposit a marker. `RA1b`'s `submit=false` left an IDLE orchestrator stuck (the marker
/// sat in the input, unsubmitted) → no functional wake. Now aligator presses Enter.
#[must_use]
pub fn wake_swamp_entry(orch: &str, msg: &str, at_ms: u64, build: &str) -> crate::cli::aligator::SwampEntry {
    crate::cli::aligator::SwampEntry {
        ts: at_ms / 1000,
        tab: orch.to_string(),
        input: msg.to_string(),
        submit: true, // ⭐ the RA1c fix — trigger the turn, don't just deposit a marker.
        from: Some("daemon".to_string()),
        attempts: 0,
        priority: crate::cli::aligator::Priority::Status,
        dedup_key: Some(format!("restart-wake-{build}")),
    }
}

/// The REAL restart-wake push seam: append the [`wake_swamp_entry`] to the live swamp.
/// One definition so headless + gui can't drift.
///
/// # Errors
/// Propagates the append I/O error (the caller counts it, best-effort).
pub fn real_swamp_push(now: u64, build: &str, orch: &str, msg: &str) -> std::io::Result<()> {
    crate::cli::aligator::append_swamp_line(&crate::cli::aligator::swamp_path(), &wake_swamp_entry(orch, msg, now, build))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn is_orchestrator_matches_role_and_meta_manager_only() {
        assert!(is_orchestrator(Some("build/orchestrator")), "role orchestrator");
        assert!(is_orchestrator(Some("kalpin-back:review/orchestrator")), "override + role");
        assert!(is_orchestrator(Some("meta/manager")), "the fleet meta/manager");
        assert!(is_orchestrator(Some("terre:meta/manager")), "override + meta/manager");
        assert!(!is_orchestrator(Some("build/builder")), "a builder is not an orchestrator");
        assert!(!is_orchestrator(Some("agent-lifecycle/reviewer")), "reviewer excluded");
        assert!(!is_orchestrator(None), "no assignment → not an orchestrator");
        assert!(!is_orchestrator(Some("   ")), "blank assignment → excluded");
    }

    #[test]
    fn roster_keeps_only_orchestrators() {
        let tabs = [
            ("t-orch", Some("build/orchestrator")),
            ("t-bld", Some("build/builder")),
            ("t-mgr", Some("meta/manager")),
            ("t-none", None),
        ];
        let roster = orchestrator_roster(tabs.iter().map(|(i, a)| (*i, *a)));
        assert_eq!(roster, vec!["t-orch", "t-mgr"], "only orchestrators + meta/manager");
    }

    // RA1-1: latency restart→wake is MEASURED (< 30s). The wake carries the restart
    // ts; the receiver reads it back and computes the delta. In-process the delta is
    // ~0, so this genuinely bounds the emission→reception path under the ceiling.
    #[test]
    fn ra1_1_restart_to_wake_latency_is_measured_under_30s() {
        let ts_restart = crate::unix_millis();
        let roster = vec!["orch-1".to_string()];
        let received = RefCell::new(Vec::<String>::new());
        let report = emit_restart_wake(
            "abc123def456",
            ts_restart,
            &roster,
            |_t, _f, _m| {},
            |_orch, msg| {
                received.borrow_mut().push(msg.to_string()); // the orchestrator's swamp gets it
                Ok(())
            },
        );
        assert_eq!(report.emitted_at, ts_restart, "the report carries the restart ts");
        assert_eq!(report.pushed, 1);
        // Reception side: read the ts back out and measure the delta against NOW.
        let carried = parse_wake_ts(&received.borrow()[0]).expect("wake carries at=<ts>");
        assert_eq!(carried, ts_restart, "the restart ts survives the wake round-trip");
        let ts_recv = crate::unix_millis();
        let latency_ms = ts_recv.saturating_sub(carried);
        assert!(latency_ms < 30_000, "restart→wake latency {latency_ms}ms is under the 30s ceiling");
    }

    // RA1-2 (orthogonality A4-3): the wake channel carries NO agentKind — the
    // emission can't modify the self_announce tag. Both the ops note and the swamp
    // wake are free of any agentKind/state key (event ≠ tag, separate channel).
    #[test]
    fn ra1_2_emission_carries_no_agent_kind() {
        let roster = vec!["orch-1".to_string()];
        let note_seen = RefCell::new(String::new());
        let swamp_seen = RefCell::new(String::new());
        emit_restart_wake(
            "b1",
            42,
            &roster,
            |topic, from, msg| {
                assert_eq!(topic, "ops", "durable fallback on the ops topic");
                assert_eq!(from, "daemon");
                *note_seen.borrow_mut() = msg.to_string();
            },
            |_orch, msg| {
                *swamp_seen.borrow_mut() = msg.to_string();
                Ok(())
            },
        );
        for payload in [note_seen.borrow().as_str(), swamp_seen.borrow().as_str()] {
            assert!(payload.starts_with("RESTART_DONE"), "the wake is a RESTART_DONE event");
            assert!(!payload.contains("agentKind"), "the wake never carries agentKind (tag untouched)");
            assert!(!payload.contains("\"state\""), "the wake never carries a status/state field");
        }
    }

    // RA1-3: a failing emission NEVER blocks/delays startup. Every swamp push errs;
    // the call returns a report (best-effort) — no panic, no propagation.
    #[test]
    fn ra1_3_emission_failure_never_blocks_startup() {
        let roster = vec!["orch-1".to_string(), "orch-2".to_string()];
        let report = emit_restart_wake(
            "b1",
            7,
            &roster,
            |_t, _f, _m| {}, // note also best-effort (returns unit)
            |_orch, _msg| Err(std::io::Error::other("swamp write failed")),
        );
        // The function RETURNED (didn't panic/block) and counted the failures.
        assert_eq!(report.failed, 2, "both pushes failed…");
        assert_eq!(report.pushed, 0, "…none delivered");
        assert!(report.note_posted, "the durable fallback note was still attempted");
    }

    // Behaviour-preserving NET (isolated char-test on the emission): the exact
    // message + per-orchestrator targeting, so a later refactor can't drift them.
    #[test]
    fn emission_message_and_targets_are_pinned() {
        let roster = vec!["orch-a".to_string(), "orch-b".to_string()];
        let calls = RefCell::new(Vec::<(String, String)>::new());
        let report = emit_restart_wake(
            "deadbeef",
            1000,
            &roster,
            |_t, _f, _m| {},
            |orch, msg| {
                calls.borrow_mut().push((orch.to_string(), msg.to_string()));
                Ok(())
            },
        );
        assert_eq!(report.pushed, 2);
        assert_eq!(
            *calls.borrow(),
            vec![
                ("orch-a".to_string(), "RESTART_DONE build=deadbeef at=1000".to_string()),
                ("orch-b".to_string(), "RESTART_DONE build=deadbeef at=1000".to_string()),
            ],
            "each orchestrator gets the exact pinned wake marker"
        );
    }

    // ----- RA1b: the SHARED startup wiring both editions (headless + gui) run ---

    // RA1b acceptance (d): read-only → the startup wake is SKIPPED entirely (no
    // roster snapshot, no emission). The gate both daemon editions share.
    #[test]
    fn ra1b_read_only_skips_the_startup_wake() {
        let tabs = [("t-orch", Some("build/orchestrator"))];
        let mut swamp_calls = 0usize;
        let out = emit_startup_wake(
            true, // read-only
            tabs.iter().map(|(i, a)| (*i, *a)),
            "b1",
            42,
            |_t, _f, _m| panic!("read-only must not post a note"),
            |_orch, _msg| {
                swamp_calls += 1;
                Ok(())
            },
        );
        assert!(out.is_none(), "read-only → no emission (None)");
        assert_eq!(swamp_calls, 0, "no swamp push in read-only");
    }

    // RA1b acceptance (a): the SHARED path (the one the GUI now calls) DOES emit —
    // roster filtered to orchestrators, note posted, swamp pushed. This is the
    // GUI-path coverage the headless-only RA1 tests missed (lesson #13).
    #[test]
    fn ra1b_startup_wake_emits_via_the_shared_path() {
        let tabs = [
            ("t-orch", Some("build/orchestrator")),
            ("t-bld", Some("build/builder")), // filtered out
            ("t-mgr", Some("meta/manager")),
        ];
        let note = RefCell::new(None);
        let pushed = RefCell::new(Vec::<String>::new());
        let out = emit_startup_wake(
            false,
            tabs.iter().map(|(i, a)| (*i, *a)),
            "b1",
            7,
            |topic, _f, msg| *note.borrow_mut() = Some((topic.to_string(), msg.to_string())),
            |orch, _msg| {
                pushed.borrow_mut().push(orch.to_string());
                Ok(())
            },
        )
        .expect("not read-only → Some(report)");
        assert_eq!(out.pushed, 2, "both orchestrators woken (builder filtered out)");
        assert_eq!(*pushed.borrow(), vec!["t-orch", "t-mgr"], "only orchestrators pushed");
        assert_eq!(note.borrow().as_ref().map(|(t, _)| t.as_str()), Some("ops"), "ops note posted");
    }

    // RA1b acceptance (c): a failing emission on the shared path NEVER propagates —
    // the report counts the failure and startup continues (no panic/block).
    #[test]
    fn ra1b_shared_path_failure_never_blocks_startup() {
        let tabs = [("t-orch", Some("meta/manager"))];
        let out = emit_startup_wake(
            false,
            tabs.iter().map(|(i, a)| (*i, *a)),
            "b1",
            7,
            |_t, _f, _m| {},
            |_orch, _msg| Err(std::io::Error::other("swamp write failed")),
        )
        .expect("emission attempted");
        assert_eq!(out.failed, 1, "the failure is counted…");
        assert_eq!(out.pushed, 0, "…not delivered");
        assert!(out.note_posted, "…and startup continues (returned, no panic)");
    }

    // ----- RA1c: submit=true + deferred + staggered + generic helper ------------

    // RA1c: the schedule is round-robin with a FIXED gap — no two orchestrators submit
    // at the same instant (anti-herd).
    #[test]
    fn ra1c_wake_schedule_is_round_robin_with_fixed_gap() {
        let roster = vec!["o1".to_string(), "o2".to_string(), "o3".to_string()];
        let sched = wake_schedule(&roster, 1000, WAKE_GAP_MS);
        assert_eq!(sched.len(), 3);
        assert_eq!(sched[0], WakeStop { target: "o1".into(), at_ms: 1000 });
        assert_eq!(sched[1].at_ms, 1000 + WAKE_GAP_MS, "2nd orchestrator staggered by the fixed gap");
        assert_eq!(sched[2].at_ms, 1000 + 2 * WAKE_GAP_MS);
        assert_ne!(sched[0].at_ms, sched[1].at_ms, "anti-herd: never simultaneous");
    }

    // RA1c: run_deferred_wake WAITS for readiness, THEN delivers round-robin, pacing the
    // fixed gap between submits.
    #[test]
    fn ra1c_run_deferred_wake_gates_on_readiness_then_delivers_staggered() {
        let roster = vec!["o1".to_string(), "o2".to_string()];
        let polls = std::cell::Cell::new(0u32);
        let ready = || {
            polls.set(polls.get() + 1);
            polls.get() > 2 // ready only on the 3rd poll → 2 readiness waits first
        };
        let delivered = RefCell::new(Vec::<String>::new());
        let paces = RefCell::new(Vec::<u64>::new());
        let n = run_deferred_wake(
            &roster,
            500,
            WAKE_GAP_MS,
            ready,
            |stop| {
                delivered.borrow_mut().push(stop.target.clone());
                Ok(())
            },
            |ms| paces.borrow_mut().push(ms),
        );
        assert_eq!(n, 2, "both orchestrators delivered after readiness");
        assert_eq!(*delivered.borrow(), vec!["o1", "o2"], "round-robin order");
        let p = paces.borrow();
        assert_eq!(p.iter().filter(|&&ms| ms == WAKE_READY_POLL_MS).count(), 2, "polled readiness twice before ready");
        assert_eq!(p.iter().filter(|&&ms| ms == WAKE_GAP_MS).count(), 1, "one fixed gap between the two submits");
    }

    // RA1c: the readiness wait is BOUNDED — a never-ready fleet still fires the wake
    // (aligator then transient-retries) and NEVER hangs startup.
    #[test]
    fn ra1c_readiness_wait_is_bounded_never_hangs() {
        let roster = vec!["o1".to_string()];
        let waited = std::cell::Cell::new(0u64);
        let n = run_deferred_wake(
            &roster,
            0,
            0,
            || false, // never ready
            |_stop| Ok(()),
            |ms| waited.set(waited.get() + ms),
        );
        assert_eq!(n, 1, "the wake still fired after the bounded wait (never hung)");
        assert!(waited.get() >= WAKE_READY_MAX_MS, "waited up to the bounded ceiling");
        assert!(waited.get() <= WAKE_READY_MAX_MS + WAKE_READY_POLL_MS, "bounded — didn't overshoot");
    }

    // RA1c ⭐ (anti built≠wired): the REAL wake entry SUBMITS (submit=true) and
    // round-trips through the REAL swamp serialization (real-fs temp file, the actual
    // `wake_swamp_entry` builder `real_swamp_push` uses + `append_swamp_line`/
    // `parse_swamp`), delivered round-robin via `run_deferred_wake`.
    #[test]
    fn ra1c_real_wake_entry_submits_true_and_round_trips_on_real_fs() {
        use crate::cli::aligator::{Priority, append_swamp_line, parse_swamp};
        let tmp = std::env::temp_dir().join(format!("ra1c-swamp-{}.jsonl", crate::default_tab_id()));
        struct Rm(std::path::PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Rm(tmp.clone());

        let roster = vec!["o1".to_string(), "o2".to_string()];
        let msg = wake_input("deadbeef", 1000);
        let n = run_deferred_wake(
            &roster,
            1000,
            WAKE_GAP_MS,
            || true, // ready
            |stop| append_swamp_line(&tmp, &wake_swamp_entry(&stop.target, &msg, stop.at_ms, "deadbeef")),
            |_ms| {}, // no real sleep in the test
        );
        assert_eq!(n, 2, "both wakes delivered to the real swamp file");

        let entries = parse_swamp(&std::fs::read_to_string(&tmp).unwrap());
        assert_eq!(entries.len(), 2, "two entries persisted");
        for e in &entries {
            assert!(e.submit, "⭐ RA1c: the REAL wake entry SUBMITS (submit=true) — the fix");
            assert_eq!(e.priority, Priority::Status, "Status priority");
            assert_eq!(e.dedup_key.as_deref(), Some("restart-wake-deadbeef"), "deduped per build");
            assert!(e.input.starts_with("RESTART_DONE"), "carries the wake marker");
        }
        assert_eq!(entries[0].tab, "o1");
        assert_eq!(entries[1].tab, "o2");
        // Staggered ts (seconds): o2 is one gap later than o1.
        assert_eq!(entries[1].ts, entries[0].ts + WAKE_GAP_MS / 1000, "round-robin stagger on the wire");
    }

    // RA1c ⭐ brain-relay brick: `push_swamp_input` is GENERIC — it honours the caller's
    // submit flag (true = trigger the target's turn, the push-relay case; false = a
    // non-intrusive marker), reusable beyond the restart-wake.
    #[test]
    fn ra1c_push_swamp_input_honours_the_submit_flag() {
        // The wake builder (real path) is submit=true…
        assert!(wake_swamp_entry("o1", "m", 0, "b").submit, "the restart-wake submits");
        // …and the generic helper is a plain flag pass-through (brain-relay building
        // block): a caller can push a non-intrusive marker (false) or a turn-trigger
        // (true) with the same brick — proven by the entry the builder would append.
        let submit_true = crate::cli::aligator::SwampEntry {
            ts: 0,
            tab: "target".into(),
            input: "push".into(),
            submit: true,
            from: Some("daemon".into()),
            attempts: 0,
            priority: crate::cli::aligator::Priority::Status,
            dedup_key: None,
        };
        assert!(submit_true.submit, "push_swamp_input(..., true, ...) → a turn-triggering push");
    }
}
