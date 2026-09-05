// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier backlog` — turn measured state into announced work.
//!
//! A fleet that waits for a human to type tasks isn't self-organising, it's
//! just remote control. This is the generator: it reads a coverage report,
//! finds the files that are worst covered and the ones that haven't been
//! audited lately, and announces them on the blackboard. Agents then take
//! work with `take`, which needs no scheduler and no supervisor.
//!
//! Two properties make it safe to run on a timer:
//!
//! - **Idempotent.** Task ids are derived from the target (`cov:src/api.rs`),
//!   so re-running announces nothing new while the task is still open. The
//!   blackboard grows with the work, not with the polling.
//! - **Cooling.** A finished task isn't re-announced until `--cooldown` days
//!   have passed, so the fleet moves on instead of re-auditing yesterday's
//!   file forever.
//!
//! Coverage comes from LCOV (`cargo llvm-cov --lcov --output-path …`) rather
//! than by shelling out to cargo: generating a report is a multi-minute build,
//! which is the caller's business, not something a backlog sweep should
//! trigger.

use super::tasks::{TaskState, fold_tasks};
use super::team::{NoteKind, append_entry, new_entry, read_blackboard};

/// Per-file line coverage, as LCOV reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCoverage {
    pub path: String,
    pub lines_found: u32,
    pub lines_hit: u32,
}

impl FileCoverage {
    /// Percent of lines hit, 100 for a file with nothing to cover (so an empty
    /// file never looks like the worst in the tree).
    #[must_use]
    pub fn percent(&self) -> f64 {
        if self.lines_found == 0 {
            return 100.0;
        }
        f64::from(self.lines_hit) * 100.0 / f64::from(self.lines_found)
    }

    /// Lines that would have to be covered to reach `target` percent — the
    /// natural size estimate for the task, and a usable bid cost.
    #[must_use]
    pub fn lines_to(&self, target: f64) -> u32 {
        let want = (f64::from(self.lines_found) * target / 100.0).ceil();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "want is a ceil of a non-negative product bounded by lines_found, so it fits u32"
        )]
        let want = want as u32;
        want.saturating_sub(self.lines_hit)
    }
}

/// Parse the subset of LCOV we need: `SF:` path, `LF:`/`LH:` counts.
///
/// Deliberately lenient — an unfamiliar record type is skipped rather than
/// failing the sweep, because a coverage tool adding a field must not stop the
/// fleet from finding work.
#[must_use]
pub fn parse_lcov(body: &str) -> Vec<FileCoverage> {
    let mut out = Vec::new();
    let mut cur: Option<FileCoverage> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            cur = Some(FileCoverage {
                path: path.to_owned(),
                lines_found: 0,
                lines_hit: 0,
            });
        } else if let Some(n) = line.strip_prefix("LF:") {
            if let (Some(c), Ok(v)) = (cur.as_mut(), n.parse()) {
                c.lines_found = v;
            }
        } else if let Some(n) = line.strip_prefix("LH:") {
            if let (Some(c), Ok(v)) = (cur.as_mut(), n.parse()) {
                c.lines_hit = v;
            }
        } else if line == "end_of_record"
            && let Some(c) = cur.take()
        {
            out.push(c);
        }
    }
    out
}

/// The files most worth working on: below `target` percent, biggest shortfall
/// first, skipping files too small for the effort to be worth a task.
#[must_use]
pub fn worst_covered(files: &[FileCoverage], target: f64, min_lines: u32, limit: usize) -> Vec<&FileCoverage> {
    let mut candidates: Vec<&FileCoverage> = files
        .iter()
        .filter(|f| f.lines_found >= min_lines && f.percent() < target)
        .collect();
    // Biggest absolute shortfall first — a 40%-covered 900-line file is worth
    // more than a 10%-covered 30-line one. Path breaks ties so two hosts
    // generating from the same report announce the same tasks.
    candidates.sort_by(|a, b| {
        b.lines_to(target)
            .cmp(&a.lines_to(target))
            .then_with(|| a.path.cmp(&b.path))
    });
    candidates.into_iter().take(limit).collect()
}

/// Whether `task_id` should be announced, given what the board already says.
///
/// Skips anything still open or in flight, and anything finished more recently
/// than `cooldown_s`. This is what keeps a timer-driven sweep from turning the
/// board into a pile of duplicates.
#[must_use]
pub fn should_announce(board: &[super::tasks::TaskView], task_id: &str, now_s: u64, cooldown_s: u64) -> bool {
    let Some(t) = board.iter().find(|t| t.id == task_id) else {
        return true;
    };
    match t.state() {
        // Somebody is on it, or it is waiting to be taken.
        TaskState::Open | TaskState::Bidding | TaskState::Awarded => false,
        // A failure is worth retrying once it has cooled, same as a success —
        // an agent that gave up may just have been the wrong agent.
        TaskState::Done | TaskState::Failed => now_s.saturating_sub(t.last_ts) >= cooldown_s,
    }
}

fn usage() {
    eprintln!(
        "usage: tab-atelier backlog [--lcov <path>] [--target <pct>] [--limit N]\n  \
         [--min-lines N] [--cooldown <days>] [--audit-largest N] [--dry-run]\n\n\
         Announces work derived from a coverage report: the worst-covered files, plus\n\
         a rotation of small audits. Idempotent — safe to run on a timer.\n\
         Produce the report with:\n  \
         cargo llvm-cov --lcov --output-path target/lcov.info"
    );
}

/// One planned announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub id: String,
    pub title: String,
}

/// What the sweep looks for, kept together so the policy is one value rather
/// than a handful of positional numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Coverage percentage worth aiming at.
    pub target: f64,
    /// Ignore files smaller than this — a task has overhead, and a 12-line
    /// file isn't worth an agent's context window.
    pub min_lines: u32,
    /// Most coverage tasks to announce per sweep.
    pub limit: usize,
    /// How many of the largest files to put up for audit.
    pub audit_largest: usize,
    /// How long a finished task stays off the board.
    pub cooldown_s: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            target: 80.0,
            min_lines: 40,
            limit: 5,
            audit_largest: 2,
            cooldown_s: 30 * 86_400,
        }
    }
}

/// Decide what to announce. Pure, so the policy is testable without a board on
/// disk or a coverage run.
#[must_use]
pub fn plan(files: &[FileCoverage], board: &[super::tasks::TaskView], p: &Policy, now_s: u64) -> Vec<Planned> {
    let (target, cooldown_s) = (p.target, p.cooldown_s);
    let mut out = Vec::new();
    for f in worst_covered(files, target, p.min_lines, p.limit) {
        let id = format!("cov:{}", f.path);
        if should_announce(board, &id, now_s, cooldown_s) {
            out.push(Planned {
                id,
                title: format!(
                    "raise coverage of {} — {:.1}% of {} lines, {} to reach {target:.0}%",
                    f.path,
                    f.percent(),
                    f.lines_found,
                    f.lines_to(target),
                ),
            });
        }
    }
    // Audits rotate over the biggest files: size is a decent proxy for "most
    // places to hide a bug", and it gives a stable order every host agrees on.
    let mut by_size: Vec<&FileCoverage> = files.iter().collect();
    by_size.sort_by(|a, b| b.lines_found.cmp(&a.lines_found).then_with(|| a.path.cmp(&b.path)));
    for f in by_size.into_iter().take(p.audit_largest) {
        let id = format!("audit:{}", f.path);
        if should_announce(board, &id, now_s, cooldown_s) {
            out.push(Planned {
                id,
                title: format!(
                    "small audit of {} ({} lines) — one focused pass, report what you find",
                    f.path, f.lines_found
                ),
            });
        }
    }
    out
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut lcov = "target/lcov.info".to_string();
    let mut policy = Policy::default();
    let mut cooldown_days = policy.cooldown_s / 86_400;
    let mut dry = false;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].clone();
        // Every flag below takes a value; fetch it once so a missing value is
        // one error path rather than six.
        let mut value = || -> Option<String> {
            i += 1;
            args.get(i).cloned()
        };
        let parsed = match flag.as_str() {
            "--dry-run" => {
                dry = true;
                i += 1;
                continue;
            }
            "-h" | "--help" => {
                usage();
                return 0;
            }
            "--lcov" | "--target" | "--limit" | "--min-lines" | "--cooldown" | "--audit-largest" => value(),
            other => {
                eprintln!("backlog: unknown argument: {other}");
                return 2;
            }
        };
        let Some(v) = parsed else {
            eprintln!("backlog: {flag} expects a value");
            return 2;
        };
        let ok = match flag.as_str() {
            "--lcov" => {
                lcov.clone_from(&v);
                true
            }
            "--target" => v.parse().map(|t| policy.target = t).is_ok(),
            "--limit" => v.parse().map(|l| policy.limit = l).is_ok(),
            "--min-lines" => v.parse().map(|m| policy.min_lines = m).is_ok(),
            "--cooldown" => v.parse().map(|d| cooldown_days = d).is_ok(),
            "--audit-largest" => v.parse().map(|a| policy.audit_largest = a).is_ok(),
            _ => true,
        };
        if !ok {
            eprintln!("backlog: {flag} got {v:?}, which is not a number");
            return 2;
        }
        i += 1;
    }
    policy.cooldown_s = cooldown_days.saturating_mul(86_400);

    let body = match std::fs::read_to_string(&lcov) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "backlog: {lcov}: {e}\n\
                 generate one with: cargo llvm-cov --lcov --output-path {lcov}"
            );
            return 1;
        }
    };
    let files = parse_lcov(&body);
    if files.is_empty() {
        eprintln!("backlog: {lcov} has no file records — is it LCOV?");
        return 1;
    }
    let board = fold_tasks(&read_blackboard());
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let planned = plan(&files, &board, &policy, now_s);
    if planned.is_empty() {
        println!("(nothing to announce — every candidate is already on the board or still cooling)");
        return 0;
    }
    for p in &planned {
        if dry {
            println!("would announce {} — {}", p.id, p.title);
            continue;
        }
        let mut n = new_entry(NoteKind::Announce, Some("backlog".into()), &p.title);
        n.task = Some(p.id.clone());
        match append_entry(n) {
            Ok(_) => println!("announced {}", p.id),
            Err(e) => {
                eprintln!("backlog: {e}");
                return 1;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCOV: &str = "TN:\n\
        SF:src/api.rs\n\
        DA:1,1\n\
        LF:1000\n\
        LH:400\n\
        end_of_record\n\
        SF:src/tiny.rs\n\
        LF:10\n\
        LH:0\n\
        end_of_record\n\
        SF:src/good.rs\n\
        LF:200\n\
        LH:190\n\
        end_of_record\n\
        SF:src/mid.rs\n\
        LF:300\n\
        LH:150\n\
        end_of_record\n";

    fn cov() -> Vec<FileCoverage> {
        parse_lcov(LCOV)
    }

    #[test]
    fn lcov_parses_into_per_file_counts() {
        let files = cov();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, "src/api.rs");
        assert_eq!((files[0].lines_found, files[0].lines_hit), (1000, 400));
        assert!((files[0].percent() - 40.0).abs() < f64::EPSILON);
        // Unknown records (TN:, DA:) are skipped rather than fatal.
        assert!(files.iter().all(|f| !f.path.is_empty()));
        // A record with no counts still yields a file rather than vanishing.
        let partial = parse_lcov("SF:src/x.rs\nend_of_record\n");
        assert_eq!(partial.len(), 1);
        assert!(
            (partial[0].percent() - 100.0).abs() < f64::EPSILON,
            "nothing to cover is not 0%"
        );
        // Junk is not a crash and not a phantom file.
        assert!(parse_lcov("").is_empty());
        assert!(parse_lcov("not lcov at all\n").is_empty());
        // An unterminated record is ignored — half a report is not a file.
        assert!(parse_lcov("SF:src/x.rs\nLF:10\n").is_empty());
    }

    #[test]
    fn the_worst_covered_are_ranked_by_shortfall_not_percentage() {
        let files = cov();
        let worst = worst_covered(&files, 80.0, 40, 5);
        // src/tiny.rs is 0% but only 10 lines — below min_lines, so it is not
        // work worth announcing; src/good.rs is already above target.
        let paths: Vec<&str> = worst.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/api.rs", "src/mid.rs"]);
        // 1000-line file at 40% needs 400 lines; 300-line at 50% needs 90.
        assert_eq!(worst[0].lines_to(80.0), 400);
        assert_eq!(worst[1].lines_to(80.0), 90);
        // A file already at target asks for nothing.
        assert_eq!(files[2].lines_to(80.0), 0);
        assert_eq!(worst_covered(&files, 80.0, 40, 1).len(), 1, "limit applies");
    }

    #[test]
    fn generation_is_idempotent_against_the_board() {
        let files = cov();
        let now = 1_000_000_u64;
        let pol = Policy {
            audit_largest: 1,
            ..Policy::default()
        };
        let first = plan(&files, &[], &pol, now);
        let ids: Vec<&str> = first.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["cov:src/api.rs", "cov:src/mid.rs", "audit:src/api.rs"]);
        assert!(first[0].title.contains("40.0%"), "{}", first[0].title);
        assert!(first[0].title.contains("400 to reach 80%"), "{}", first[0].title);

        // Re-running with those already on the board announces nothing — this
        // is what makes a timer-driven sweep safe.
        let board = board_with(&first, now, None);
        assert!(plan(&files, &board, &pol, now).is_empty());
    }

    /// Board entries for planned tasks, optionally completed at `done_ts`.
    fn board_with(planned: &[Planned], ts: u64, done_ts: Option<u64>) -> Vec<super::super::tasks::TaskView> {
        planned
            .iter()
            .map(|p| super::super::tasks::TaskView {
                id: p.id.clone(),
                title: p.title.clone(),
                announced_by: Some("backlog".into()),
                announced_ts: ts,
                last_ts: done_ts.unwrap_or(ts),
                bids: Vec::new(),
                awarded_to: None,
                done: done_ts.map(|_| (true, "done".to_string())),
            })
            .collect()
    }

    #[test]
    fn finished_work_is_re_announced_only_after_it_cools() {
        let files = cov();
        let now = 10_000_000_u64;
        let no_audits = Policy {
            audit_largest: 0,
            cooldown_s: 0,
            ..Policy::default()
        };
        let planned = plan(&files, &[], &no_audits, now);
        let cooling = Policy {
            audit_largest: 0,
            ..Policy::default()
        };

        // Finished an hour ago: leave it alone, the fleet has other work.
        let fresh = board_with(&planned, now - 3_600, Some(now - 3_600));
        assert!(plan(&files, &fresh, &cooling, now).is_empty());

        // Finished two months ago: coverage rots, so offer it again.
        let stale = board_with(&planned, now - 60 * 86_400, Some(now - 60 * 86_400));
        assert_eq!(plan(&files, &stale, &cooling, now).len(), planned.len());
    }

    #[test]
    fn work_in_flight_is_never_re_announced() {
        let files = cov();
        let now = 1_000_000_u64;
        let hot = Policy {
            audit_largest: 0,
            cooldown_s: 0,
            ..Policy::default()
        };
        let planned = plan(&files, &[], &hot, now);
        let mut board = board_with(&planned, now, None);
        board[0].awarded_to = Some("agent-a".into());
        board[0].bids.push(("agent-a".into(), 400));
        // Even with a zero cooldown, an awarded task is somebody's job.
        let again = plan(&files, &board, &hot, now);
        assert!(
            !again.iter().any(|p| p.id == board[0].id),
            "announced work that is already awarded"
        );
    }

    #[test]
    fn audits_rotate_over_the_biggest_files() {
        let files = cov();
        let now = 1_000_000_u64;
        // No coverage tasks (limit 0), just audits: biggest first.
        let audits_only = Policy {
            limit: 0,
            audit_largest: 2,
            ..Policy::default()
        };
        let audits = plan(&files, &[], &audits_only, now);
        let ids: Vec<&str> = audits.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["audit:src/api.rs", "audit:src/mid.rs"]);
        assert!(audits[0].title.contains("1000 lines"), "{}", audits[0].title);
    }

    #[test]
    fn args_are_validated() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(run(&argv(&["--limit", "abc"])), 2);
        assert_eq!(run(&argv(&["--target"])), 2);
        assert_eq!(run(&argv(&["--nope"])), 2);
        assert_eq!(run(&argv(&["--help"])), 0);
        // A missing report explains how to make one rather than failing bare.
        assert_eq!(run(&argv(&["--lcov", "/nonexistent/lcov.info"])), 1);
    }
}
