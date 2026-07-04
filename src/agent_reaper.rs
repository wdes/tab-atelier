// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reap orphaned agent processes the desktop GUI leaked in a prior run.
//!
//! The headless daemon reaps orphans by cgroup (`cgroup.rs`). The
//! unprivileged desktop has no cgroups: it relies on `kill_tab_pgroup`
//! at close/respawn/quit, which misses any `claude` that escaped to
//! `init` — after an unclean GUI exit (crash / SIGKILL, so `close_all_
//! tabs` never ran) the whole agent fleet reparents to `init` and, on a
//! dead PTY, wedges in job-control **stopped** state. They pile up across
//! sessions (measured: 13 ghosts, ~3.8 GB, some 3 days old, surviving
//! restarts) and the next launch `--resume`s a duplicate of a session
//! whose old copy is a stuck ghost.
//!
//! ## Zero-collateral guarantee
//!
//! We must never kill a `claude` we didn't launch (a legit one in the
//! user's own terminal). So this is **provenance-based**: every tick the
//! GUI records the `(pid, start_time)` of each agent tab's process to
//! `<state>/agent_procs.json` ([`record_live_agents`]). On the next
//! startup [`reap_orphans`] kills a process **only if** its pid is still
//! alive *and* its `/proc/<pid>/stat` start-time byte-for-byte matches a
//! recorded entry. A process we never recorded is never in the file, so
//! it can never be touched; PID reuse is defeated by the start-time
//! check (a reused pid has a different start-time ⇒ skipped).
//!
//! Overwrite-per-tick means the file is exactly "the agents alive right
//! now". After a **clean** quit those pids are dead by next startup ⇒
//! start-time can't match ⇒ nothing is reaped. After a **crash** they're
//! still alive ⇒ reaped. That's the whole design.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One agent process this GUI launched, identity-pinned by start-time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedAgent {
    /// PTY-child pid — the process is `claude` itself (agent tabs
    /// `exec claude`), and being the session/group leader `pid == pgid`.
    pub pid: u32,
    /// `starttime` (field 22 of `/proc/<pid>/stat`), clock ticks since
    /// boot. Unique per process lifetime — the anti-PID-reuse guard.
    pub start_time: u64,
    /// Session id (for logging / correlating duplicate `--resume`s).
    pub session: String,
}

/// Outcome of a reap pass, for logging.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReapReport {
    pub killed: u32,
    pub freed_mb: u64,
}

/// `<state>/agent_procs.json` — the provenance record.
#[must_use]
pub fn record_path(base: &Path) -> PathBuf {
    crate::state_dir(base).join("agent_procs.json")
}

/// `false` only when `TAB_ATELIER_AGENT_REAP` is an explicit off value —
/// the kill-switch. Default-on.
#[must_use]
pub fn reap_enabled() -> bool {
    !std::env::var("TAB_ATELIER_AGENT_REAP")
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
}

/// Overwrite the provenance record with the agents alive right now.
///
/// `agents` is `(pty_child_pid, session_id)` for every current agent tab;
/// the start-time is read here. Best-effort — a failed write just means a
/// crash before the next tick leaks (the pre-existing behaviour).
pub fn record_live_agents(base: &Path, agents: &[(u32, String)]) {
    if !reap_enabled() {
        return;
    }
    let tracked: Vec<TrackedAgent> = agents
        .iter()
        .filter_map(|(pid, session)| {
            proc_start_time(*pid).map(|start_time| TrackedAgent {
                pid: *pid,
                start_time,
                session: session.clone(),
            })
        })
        .collect();
    let path = record_path(base);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&tracked) {
        let _ = std::fs::write(&path, json);
    }
}

/// Kill agent processes leaked by a prior run.
///
/// Kills a recorded process only if it's still alive with a matching
/// start-time (⇒ provably ours, provably leaked), then clears the record.
/// The caller must not invoke it in read-only mode (an inspect-only
/// instance stays inert).
#[must_use]
pub fn reap_orphans(base: &Path) -> ReapReport {
    let mut report = ReapReport::default();
    if !reap_enabled() {
        return report;
    }
    let path = record_path(base);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return report;
    };
    let Ok(tracked): Result<Vec<TrackedAgent>, _> = serde_json::from_str(&raw) else {
        return report;
    };
    for a in &tracked {
        // Identity gate: same pid AND same start-time ⇒ the exact process
        // we launched, still alive. Anything else (dead pid, or a reused
        // pid with a different start-time) is skipped — never killed.
        if proc_start_time(a.pid) != Some(a.start_time) {
            continue;
        }
        report.freed_mb += proc_rss_mb(a.pid);
        kill_group(a.pid);
        report.killed += 1;
    }
    // Drop the record so a second startup can't re-attempt (the pids are
    // dead now anyway); the live run rewrites it on the next tick.
    let _ = std::fs::remove_file(&path);
    report
}

/// `starttime` (field 22) from `/proc/<pid>/stat`, or `None` if the
/// process is gone. Keyed off the last `)` because `comm` may hold spaces
/// and parens; fields after it start at field 3 (`state`), so field 22 is
/// index 19.
#[must_use]
fn proc_start_time(pid: u32) -> Option<u64> {
    parse_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Pure field-22 extraction from a `/proc/<pid>/stat` line.
#[must_use]
fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(19)?.parse().ok()
}

/// Resident set size in MiB from `/proc/<pid>/statm` (page 2 = resident
/// pages × 4 KiB), best-effort 0 on any miss. For the freed-memory tally.
#[must_use]
fn proc_rss_mb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| s.split_ascii_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4 / 1024)
}

/// SIGKILL the process group led by `pid` (== its pgid). SIGKILL — not
/// TERM — because ghosts sit **stopped**, where TERM only queues until
/// continued but KILL reaps immediately. `unsafe`-free: shells to
/// `kill(1)` with the `-- -PGID` form, same as [`crate::kill_tab_pgroup`].
fn kill_group(pid: u32) {
    if pid <= 1 {
        return;
    }
    let target = format!("-{pid}");
    let _ = std::process::Command::new("kill")
        .args(["-s", "KILL", "--", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_starttime_past_parenthesised_comm() {
        // field 22 (starttime) = 8971514; comm has spaces + parens.
        let stat = "42 (claude (worker)) S 1 42 42 0 -1 0 0 0 0 0 100 40 0 0 20 0 16 0 \
                    8971514 700000000 12345 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(8_971_514));
    }

    #[test]
    fn record_roundtrips_through_json() {
        let a = vec![TrackedAgent {
            pid: 1234,
            start_time: 999,
            session: "abc".into(),
        }];
        let json = serde_json::to_string(&a).unwrap();
        let back: Vec<TrackedAgent> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].pid, 1234);
        assert_eq!(back[0].start_time, 999);
    }
}
