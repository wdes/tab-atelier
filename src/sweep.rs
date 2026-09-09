// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! The periodic sweep that makes the fleet self-managing.
//!
//! Everything else in the fleet is a verb someone runs. This is the part that
//! runs on its own: every `fleet_sweep_minutes`, announce whatever the
//! configured sources propose, then gossip the board to the configured peers.
//!
//! It is deliberately thin, because the safety lives in the pieces it calls:
//! announcing is idempotent and cooled, so sweeping often costs nothing; and
//! gossip is a set union, so a round against a converged peer is a no-op. That
//! is what makes "run this forever on a timer" a reasonable thing to do at
//! all.
//!
//! **Off unless configured.** `fleet_sweep_minutes` defaults to 0, so an
//! instance that was never asked to manage itself never does. Nothing here
//! starts running because someone upgraded.
//!
//! Configure in `preferences.json`:
//!
//! ```json
//! {
//!   "fleet_sweep_minutes": 60,
//!   "fleet_sweep_sources": ["./scripts/my-tasks.sh"],
//!   "fleet_sweep_lcov": "target/lcov.info",
//!   "fleet_sweep_gossip": true
//! }
//! ```

use std::time::Duration;

/// What one sweep did, for the log line and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub announced: usize,
    /// Sources that failed — reported, never fatal.
    pub source_errors: Vec<String>,
    pub gossiped: usize,
    pub gossip_errors: Vec<String>,
}

/// Human-readable summary; the daemon logs this once per sweep.
#[must_use]
pub fn format_outcome(o: &Outcome) -> String {
    let mut parts = vec![format!("announced {}", o.announced)];
    if o.gossiped > 0 {
        parts.push(format!("gossiped {} peer(s)", o.gossiped));
    }
    for e in o.source_errors.iter().chain(&o.gossip_errors) {
        parts.push(format!("error: {e}"));
    }
    parts.join(", ")
}

/// Run one sweep against the current configuration.
#[must_use]
pub fn once() -> Outcome {
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    let mut out = Outcome::default();

    let mut candidates = Vec::new();
    for cmd in &prefs.fleet_sweep_sources {
        match crate::cli::backlog::run_source(cmd) {
            Ok(c) => candidates.extend(c),
            Err(e) => out.source_errors.push(e),
        }
    }
    if let Some(path) = prefs.fleet_sweep_lcov.as_deref().filter(|p| !p.is_empty()) {
        match std::fs::read_to_string(path) {
            Ok(body) => {
                let files = crate::cli::backlog::parse_lcov(&body);
                if files.is_empty() {
                    out.source_errors.push(format!("{path}: no LCOV records"));
                } else {
                    let policy = crate::cli::backlog::CoveragePolicy {
                        root: prefs.fleet_sweep_root.as_ref().map(std::path::PathBuf::from),
                        ..crate::cli::backlog::CoveragePolicy::default()
                    };
                    candidates.extend(crate::cli::backlog::coverage_candidates(&files, &policy));
                }
            }
            Err(e) => out.source_errors.push(format!("{path}: {e}")),
        }
    }
    if !candidates.is_empty() {
        let board = crate::cli::tasks::fold_tasks(&crate::cli::team::read_blackboard());
        let now_s = crate::unix_millis() / 1000;
        let cooldown = u64::from(prefs.fleet_sweep_cooldown_days.max(1)) * 86_400;
        for p in crate::cli::backlog::plan(&candidates, &board, now_s, cooldown) {
            let mut n =
                crate::cli::team::new_entry(crate::cli::team::NoteKind::Announce, Some("sweep".into()), &p.title);
            n.task = Some(p.id.clone());
            match crate::cli::team::append_entry(n) {
                Ok(_) => out.announced += 1,
                Err(e) => out.source_errors.push(e),
            }
        }
    }

    if prefs.fleet_sweep_gossip {
        for round in crate::cli::gossip::sweep_all() {
            match round.error {
                Some(e) => out.gossip_errors.push(format!("{}: {e}", round.peer)),
                None => out.gossiped += 1,
            }
        }
    }
    out
}

/// Longest wait before the FIRST sweep.
///
/// A daemon restarted more often than its period would otherwise never sweep
/// at all — every restart resets the timer. Waiting a little is still right:
/// startup is the worst moment to shell out to a source that might build.
const FIRST_DELAY_CAP: Duration = Duration::from_mins(2);

/// How long to wait before the next sweep, or `None` to not sweep at all.
///
/// Separated from the thread so the decision is testable without spawning
/// anything — the earlier version of this test called the spawner, which read
/// the developer's real config and would start a live sweeping thread during
/// `cargo test` on a machine where the sweep was enabled.
#[must_use]
pub fn sweep_period(minutes: u32, read_only: bool) -> Option<Duration> {
    // A read-only instance is meant to run BESIDE a normal one. Sweeping from
    // it would shell out to sources, append to the shared blackboard and
    // gossip to peers — all writes, from the instance defined by not making
    // any.
    if read_only || minutes == 0 {
        return None;
    }
    Some(Duration::from_mins(u64::from(minutes)))
}

/// Start the sweep thread if the configuration asks for one.
///
/// Called from both editions right after the API server starts. `read_only`
/// is the instance's own flag: a read-only daemon never sweeps.
pub fn spawn_if_configured(read_only: bool) {
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    let Some(period) = sweep_period(prefs.fleet_sweep_minutes, read_only) else {
        return;
    };
    let minutes = prefs.fleet_sweep_minutes;
    let spawned = std::thread::Builder::new()
        .name("tab-atelier-sweep".into())
        .spawn(move || {
            let mut next = period.min(FIRST_DELAY_CAP);
            loop {
                std::thread::sleep(next);
                // Re-read every tick so `fleet_sweep_minutes: 0` actually
                // stops the loop, rather than requiring a restart to undo.
                let prefs = crate::load_preferences(&crate::platform::config_dir());
                let Some(period) = sweep_period(prefs.fleet_sweep_minutes, read_only) else {
                    log::info!("fleet sweep: disabled in preferences, stopping");
                    return;
                };
                next = period;
                // A panic here would end the thread permanently and silently —
                // the loop is unattended, so it has to survive one bad source.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(once)) {
                    Ok(out) => {
                        if out.announced > 0 || !out.source_errors.is_empty() || !out.gossip_errors.is_empty() {
                            log::info!("fleet sweep: {}", format_outcome(&out));
                        }
                    }
                    Err(_) => log::error!("fleet sweep: panicked; continuing at the next tick"),
                }
            }
        });
    match spawned {
        Ok(_) => log::info!("fleet sweep: every {minutes} min"),
        Err(e) => log::error!("fleet sweep: could not start: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_what_happened_including_failures() {
        assert_eq!(format_outcome(&Outcome::default()), "announced 0");
        let o = Outcome {
            announced: 3,
            gossiped: 2,
            source_errors: vec!["./bad.sh: exited 1".into()],
            gossip_errors: vec!["build-box: connection refused".into()],
        };
        let s = format_outcome(&o);
        assert!(s.starts_with("announced 3, gossiped 2 peer(s)"), "{s}");
        // A silent failure in an unattended loop is the worst kind, so both
        // error classes make it into the one line the daemon logs.
        assert!(s.contains("./bad.sh"), "{s}");
        assert!(s.contains("connection refused"), "{s}");
    }

    #[test]
    fn a_sweep_is_off_unless_it_was_asked_for() {
        // The default must never start a background loop: an upgrade should
        // not make an instance begin announcing work on its own.
        let prefs = crate::Preferences::default();
        assert_eq!(prefs.fleet_sweep_minutes, 0);
        assert!(prefs.fleet_sweep_sources.is_empty());
        assert!(prefs.fleet_sweep_lcov.is_none());
        assert!(!prefs.fleet_sweep_gossip);
        assert_eq!(sweep_period(prefs.fleet_sweep_minutes, false), None);
        // NOTE: this deliberately does not call `spawn_if_configured` — that
        // reads the real user config, so on a machine where the sweep IS
        // enabled the test would start a live thread that shells out to
        // sources and gossips, from inside `cargo test`.
    }

    #[test]
    fn a_read_only_instance_never_sweeps() {
        // Read-only exists to run beside a normal instance. Sweeping from it
        // would run source commands, append to the shared blackboard and
        // gossip to peers — every one of them a write.
        assert_eq!(sweep_period(60, true), None);
        assert_eq!(sweep_period(60, false), Some(Duration::from_hours(1)));
        // Zero is off whatever the mode, and is re-read each tick so turning
        // it off in preferences stops the loop without a restart.
        assert_eq!(sweep_period(0, false), None);
        // The first wait is capped: a daemon restarted more often than its
        // period would otherwise never sweep at all.
        assert!(FIRST_DELAY_CAP < Duration::from_hours(1));
        assert_eq!(
            sweep_period(1, false).map(|p| p.min(FIRST_DELAY_CAP)),
            Some(Duration::from_mins(1)),
            "a short period is used as-is, not padded up to the cap"
        );
    }
}
