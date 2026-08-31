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

/// The REAL note seam (durable pull fallback) — one line so both editions share it.
pub fn real_note(topic: &str, from: &str, msg: &str) {
    crate::cli::team::note_best_effort(Some(topic.to_string()), Some(from.to_string()), msg);
}

/// The REAL swamp-wake push seam: enqueue a non-intrusive `Status` marker toward an
/// orchestrator, deduped per build. One definition so headless + gui can't drift.
///
/// # Errors
/// Propagates the append I/O error (the caller counts it, best-effort).
pub fn real_swamp_push(now: u64, build: &str, orch: &str, msg: &str) -> std::io::Result<()> {
    let entry = crate::cli::aligator::SwampEntry {
        ts: now / 1000,
        tab: orch.to_string(),
        input: msg.to_string(),
        // A non-intrusive marker (not auto-submitted); the orchestrator's loop
        // picks it up. ponytail: a true-submit nudge is a tuning knob.
        submit: false,
        from: Some("daemon".to_string()),
        attempts: 0,
        priority: crate::cli::aligator::Priority::Status,
        dedup_key: Some(format!("restart-wake-{build}")),
    };
    crate::cli::aligator::append_swamp_line(&crate::cli::aligator::swamp_path(), &entry)
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
}
