// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

/// Start the sweep thread if `fleet_sweep_minutes` is non-zero.
///
/// Called from both editions right after the API server starts, because a
/// sweep needs the local API for nothing but is pointless without a running
/// instance to hold the board.
pub fn spawn_if_configured() {
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    let minutes = prefs.fleet_sweep_minutes;
    if minutes == 0 {
        return;
    }
    let period = Duration::from_secs(u64::from(minutes) * 60);
    let spawned = std::thread::Builder::new()
        .name("tab-atelier-sweep".into())
        .spawn(move || {
            // One period before the first sweep: starting up is the worst
            // moment to run a source that might shell out to a build.
            loop {
                std::thread::sleep(period);
                let out = once();
                if out.announced > 0 || !out.source_errors.is_empty() || !out.gossip_errors.is_empty() {
                    log::info!("fleet sweep: {}", format_outcome(&out));
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
        // Calling the starter with that config is a no-op rather than a
        // thread that wakes every zero seconds.
        spawn_if_configured();
    }
}
