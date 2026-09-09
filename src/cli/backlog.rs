// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier backlog` — turn measured state into announced work.
//!
//! A fleet that waits for a human to type tasks isn't self-organising, it's
//! just remote control. This is the generator, and it is deliberately not
//! about any particular kind of work: a **source** is anything that prints
//! candidate tasks, one per line, as `id<TAB>title`.
//!
//! ```text
//!   tab-atelier backlog --from ./scripts/tasks-from-todo.sh
//!   tab-atelier backlog --from-file backlog.tsv
//!   tab-atelier backlog --lcov target/lcov.info      # built-in coverage source
//! ```
//!
//! Sources compose — pass `--from` several times and every candidate lands on
//! one board. What this module actually owns is the part every source needs
//! and none should reimplement:
//!
//! - **Idempotence.** A candidate whose id is already open, bidding or awarded
//!   is not announced again, so a sweep can run on a timer without turning the
//!   board into a pile of duplicates. Sources are therefore free to emit their
//!   whole world every time; they don't have to remember what they said.
//! - **Cooling.** A finished task stays off the board for `--cooldown` days, so
//!   the fleet moves on instead of re-doing yesterday's work forever.
//!
//! That division is what makes the mechanism general: the source decides what
//! is worth doing, the backlog decides whether it is worth *saying*.

use std::process::Command;

use super::tasks::{TaskState, TaskView, fold_tasks};
use super::team::{NoteKind, append_entry, new_entry, read_blackboard};

/// One piece of work a source thinks is worth doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Stable across runs — this is what makes re-running safe. Derive it from
    /// the thing being worked on (`cov:src/api.rs`, `todo:src/app.rs:412`),
    /// never from a timestamp or a counter.
    pub id: String,
    pub title: String,
}

/// Parse a source's output: `id<TAB>title` per line.
///
/// Blank lines and `#` comments are skipped so a source can be a readable
/// file. A line with no tab is taken as an id whose title repeats it, because
/// a bare list of ids is a reasonable thing to emit and failing on it would be
/// pedantry. Whitespace-only ids are dropped rather than announced as an
/// unnameable task.
#[must_use]
pub fn parse_candidates(text: &str) -> Vec<Candidate> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|line| {
            // Split BEFORE trimming the line: a leading tab means "no id", and
            // trimming first would silently promote the title to an id.
            let (id, title) = line
                .split_once('\t')
                .map_or_else(|| (line.trim(), line.trim()), |(a, b)| (a.trim(), b.trim()));
            (!id.is_empty()).then(|| Candidate {
                id: id.to_owned(),
                title: if title.is_empty() {
                    id.to_owned()
                } else {
                    title.to_owned()
                },
            })
        })
        .collect()
}

/// Run a source command through the shell and parse its output.
///
/// Shell rather than exec so a source can be a pipeline — which is most of
/// them in practice (`rg -n TODO src | head -20 | awk …`). A source that fails
/// is reported and skipped, never fatal: one broken generator must not stop
/// the others from finding work.
///
/// # Errors
/// When the command can't be spawned, or exits non-zero (stderr is quoted).
pub fn run_source(cmd: &str) -> Result<Vec<Candidate>, String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{cmd}: exited {}: {}", out.status, err.trim()));
    }
    Ok(parse_candidates(&String::from_utf8_lossy(&out.stdout)))
}

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

/// How the built-in coverage source picks files.
#[derive(Debug, Clone, PartialEq)]
pub struct CoveragePolicy {
    /// Repository root, stripped from LCOV's absolute paths so task ids are
    /// the same in every checkout. Defaults to the process's cwd.
    pub root: Option<std::path::PathBuf>,
    /// Coverage percentage worth aiming at.
    pub target: f64,
    /// Ignore files smaller than this — a task has overhead, and a 12-line
    /// file isn't worth an agent's context window.
    pub min_lines: u32,
    /// Most coverage tasks to emit.
    pub limit: usize,
    /// How many of the largest files to put up for audit.
    pub audit_largest: usize,
}

impl Default for CoveragePolicy {
    fn default() -> Self {
        Self {
            root: None,
            target: 80.0,
            min_lines: 40,
            limit: 5,
            audit_largest: 2,
        }
    }
}

/// Path as it should appear in a task id: relative to `root` when it is
/// inside it.
///
/// LCOV records absolute paths. Left alone, the same file checked out at
/// `/home/me/proj` and `/srv/build/proj` becomes two different task ids, so
/// two machines in one fleet would each announce and each do the work — the
/// exact duplication the whole design exists to prevent. Relative ids are
/// stable across checkouts.
#[must_use]
pub fn relative_to(path: &str, root: Option<&std::path::Path>) -> String {
    root.and_then(|r| std::path::Path::new(path).strip_prefix(r).ok())
        .map_or_else(|| path.to_owned(), |p| p.to_string_lossy().into_owned())
}

/// The built-in coverage source: worst-covered files, plus a rotation of
/// audits over the largest ones.
///
/// It is only a source. Everything that makes the sweep safe to repeat lives
/// in [`plan`], which knows nothing about coverage.
#[must_use]
pub fn coverage_candidates(files: &[FileCoverage], p: &CoveragePolicy) -> Vec<Candidate> {
    // The daemon's periodic sweep runs from wherever it was started, not from
    // the repository, so the root has to be stated rather than inferred from
    // the process's cwd.
    let root = p.root.clone().or_else(|| std::env::current_dir().ok());
    let rel = |path: &str| relative_to(path, root.as_deref());
    let mut out: Vec<Candidate> = worst_covered(files, p.target, p.min_lines, p.limit)
        .into_iter()
        .map(|f| {
            let path = rel(&f.path);
            Candidate {
                id: format!("cov:{path}"),
                title: format!(
                    "raise coverage of {path} — {:.1}% of {} lines, {} to reach {:.0}%",
                    f.percent(),
                    f.lines_found,
                    f.lines_to(p.target),
                    p.target,
                ),
            }
        })
        .collect();
    // Size is a rough proxy for "most places to hide a bug", and it gives a
    // stable order every host agrees on.
    let mut by_size: Vec<&FileCoverage> = files.iter().collect();
    by_size.sort_by(|a, b| b.lines_found.cmp(&a.lines_found).then_with(|| a.path.cmp(&b.path)));
    out.extend(by_size.into_iter().take(p.audit_largest).map(|f| {
        let path = rel(&f.path);
        Candidate {
            id: format!("audit:{path}"),
            title: format!(
                "small audit of {path} ({} lines) — one focused pass, report what you find",
                f.lines_found
            ),
        }
    }));
    out
}

/// One planned announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub id: String,
    pub title: String,
}

/// Filter candidates down to what is worth announcing.
///
/// This is the whole reusable part, and it is source-agnostic on purpose:
/// drop anything already in flight, drop anything finished more recently than
/// `cooldown_s`, and de-duplicate ids within the batch so two sources
/// proposing the same work announce it once.
#[must_use]
pub fn plan(candidates: &[Candidate], board: &[TaskView], now_s: u64, cooldown_s: u64) -> Vec<Planned> {
    let mut seen = std::collections::HashSet::new();
    candidates
        .iter()
        .filter(|c| seen.insert(c.id.clone()))
        .filter(|c| should_announce(board, &c.id, now_s, cooldown_s))
        .map(|c| Planned {
            id: c.id.clone(),
            title: c.title.clone(),
        })
        .collect()
}

/// `tab-atelier backlog [sources] [--cooldown <days>] [--dry-run]`
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier backlog",
    about = "Announce work from any source",
    after_help = "Announcing is idempotent: a candidate already open (or finished within\n\
                  --cooldown days) is skipped, so a source can emit its whole world every\n\
                  run and this can go on a timer.\n\n\
                  examples:\n  \
                  tab-atelier backlog --from 'rg -n \"TODO\" src | head -20'\n  \
                  cargo llvm-cov --lcov --output-path target/lcov.info && \\\n    \
                  tab-atelier backlog --lcov target/lcov.info"
)]
struct Cli {
    /// Run it; each stdout line is `id<TAB>title`. Repeatable.
    #[arg(long = "from", value_name = "COMMAND")]
    sources: Vec<String>,
    /// Same format, read from a file. Repeatable.
    #[arg(long = "from-file", value_name = "PATH")]
    files: Vec<String>,
    /// Built-in source: worst-covered files plus a rotation of audits.
    #[arg(long, value_name = "PATH")]
    lcov: Option<String>,
    /// Coverage percentage worth aiming at.
    #[arg(long, value_name = "PCT", default_value_t = CoveragePolicy::default().target)]
    target: f64,
    /// Most coverage tasks to emit.
    #[arg(long, value_name = "N", default_value_t = CoveragePolicy::default().limit)]
    limit: usize,
    /// Ignore files smaller than this.
    #[arg(long, value_name = "N", default_value_t = CoveragePolicy::default().min_lines)]
    min_lines: u32,
    /// How many of the largest files to put up for audit.
    #[arg(long, value_name = "N", default_value_t = CoveragePolicy::default().audit_largest)]
    audit_largest: usize,
    /// Strip this prefix from LCOV paths. Defaults to the cwd.
    #[arg(long, value_name = "DIR")]
    root: Option<std::path::PathBuf>,
    /// Skip candidates finished within this many days.
    #[arg(long, value_name = "DAYS", default_value_t = 30)]
    cooldown: u64,
    /// Show what would be announced, announce nothing.
    #[arg(long)]
    dry_run: bool,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier backlog", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Cli {
        sources,
        files,
        lcov,
        target,
        limit,
        min_lines,
        audit_largest,
        root,
        cooldown: cooldown_days,
        dry_run: dry,
    } = cli;
    let policy = CoveragePolicy {
        root,
        target,
        min_lines,
        limit,
        audit_largest,
    };
    if sources.is_empty() && files.is_empty() && lcov.is_none() {
        eprintln!("backlog: nothing to read — pass --from, --from-file or --lcov (see --help)");
        return 2;
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut had_error = false;
    for cmd in &sources {
        match run_source(cmd) {
            Ok(c) => candidates.extend(c),
            Err(e) => {
                // One broken generator must not stop the others: the fleet
                // should still get whatever work the healthy sources found.
                eprintln!("backlog: source failed: {e}");
                had_error = true;
            }
        }
    }
    for path in &files {
        match std::fs::read_to_string(path) {
            Ok(body) => candidates.extend(parse_candidates(&body)),
            Err(e) => {
                eprintln!("backlog: {path}: {e}");
                had_error = true;
            }
        }
    }
    if let Some(path) = &lcov {
        match std::fs::read_to_string(path) {
            Ok(body) => {
                let files = parse_lcov(&body);
                if files.is_empty() {
                    eprintln!("backlog: {path} has no file records — is it LCOV?");
                    had_error = true;
                } else {
                    candidates.extend(coverage_candidates(&files, &policy));
                }
            }
            Err(e) => {
                eprintln!(
                    "backlog: {path}: {e}\n\
                     generate one with: cargo llvm-cov --lcov --output-path {path}"
                );
                had_error = true;
            }
        }
    }
    if candidates.is_empty() {
        eprintln!("backlog: no candidates from any source");
        return i32::from(had_error);
    }

    let board = fold_tasks(&read_blackboard());
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let planned = plan(&candidates, &board, now_s, cooldown_days.saturating_mul(86_400));
    if planned.is_empty() {
        println!("(nothing to announce — every candidate is already on the board or still cooling)");
        return i32::from(had_error);
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
    i32::from(had_error)
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

    fn cand(id: &str) -> Candidate {
        Candidate {
            id: id.to_owned(),
            title: format!("do {id}"),
        }
    }

    /// Board entries for ids, optionally completed at `done_ts`.
    fn board_with(ids: &[&str], ts: u64, done_ts: Option<u64>) -> Vec<TaskView> {
        ids.iter()
            .map(|id| TaskView {
                id: (*id).to_owned(),
                title: String::new(),
                announced_by: Some("backlog".into()),
                home: Some("test-host".into()),
                announced_ts: ts,
                last_ts: done_ts.unwrap_or(ts),
                bids: Vec::new(),
                awarded_to: None,
                done: done_ts.map(|_| (true, "done".to_string())),
            })
            .collect()
    }

    #[test]
    fn any_source_can_feed_the_board() {
        // The general contract: `id<TAB>title`, one per line. Nothing about
        // this is coverage-specific.
        let c = parse_candidates(
            "todo:src/app.rs:412\ttidy the TODO at app.rs:412\n\
             \n\
             # a comment, and the blank line above\n\
             flaky:tests/net.rs\tthis test failed 3 of 10 runs\n\
               spaced:id  \t  spaced title  \n",
        );
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].id, "todo:src/app.rs:412");
        assert_eq!(c[0].title, "tidy the TODO at app.rs:412");
        assert_eq!(c[2].id, "spaced:id", "surrounding whitespace trimmed");
        assert_eq!(c[2].title, "spaced title");
        // A bare id is a reasonable thing to emit; failing on it would be
        // pedantry, so the id doubles as the title.
        let bare = parse_candidates("just-an-id\n");
        assert_eq!(bare[0].id, "just-an-id");
        assert_eq!(bare[0].title, "just-an-id");
        // An unnameable task is dropped rather than announced.
        assert!(parse_candidates("\t title with no id\n").is_empty());
        assert!(parse_candidates("").is_empty());
    }

    #[test]
    fn a_source_command_is_run_through_the_shell() {
        // Sources are pipelines in practice, so the shell is the interface.
        let c = run_source("printf 'a\\tdo a\\nb\\tdo b\\n'").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].id, "b");
        assert!(run_source("printf 'x\\tdo x\\n' | cat").is_ok(), "pipelines work");
        // A failing source reports rather than pretending it found nothing —
        // silence would look identical to "no work", which is a lie.
        let err = run_source("echo boom >&2; exit 3").unwrap_err();
        assert!(err.contains("boom"), "{err}");
        assert!(err.contains("exited"), "{err}");
    }

    #[test]
    fn planning_is_idempotent_and_source_agnostic() {
        let now = 1_000_000_u64;
        let candidates = vec![cand("todo:a"), cand("cov:src/api.rs"), cand("audit:x")];
        let all = plan(&candidates, &[], now, 30 * 86_400);
        assert_eq!(all.len(), 3, "a fresh board takes everything");

        // Already announced: say nothing. This is what makes a timer safe, and
        // it means a source can emit its whole world every run.
        let board = board_with(&["todo:a", "cov:src/api.rs", "audit:x"], now, None);
        assert!(plan(&candidates, &board, now, 30 * 86_400).is_empty());

        // Two sources proposing the same work announce it once.
        let dupes = vec![cand("todo:a"), cand("todo:a"), cand("todo:b")];
        let ids: Vec<String> = plan(&dupes, &[], now, 0).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["todo:a", "todo:b"]);
    }

    #[test]
    fn work_in_flight_is_never_re_announced() {
        let now = 1_000_000_u64;
        let candidates = vec![cand("todo:a")];
        let mut board = board_with(&["todo:a"], now, None);
        board[0].awarded_to = Some("agent-a".into());
        // Even with a zero cooldown, an awarded task is somebody's job.
        assert!(plan(&candidates, &board, now, 0).is_empty());
        board[0].awarded_to = None;
        board[0].bids.push(("agent-a".into(), 5));
        assert!(plan(&candidates, &board, now, 0).is_empty(), "bidding is in flight too");
    }

    #[test]
    fn finished_work_is_re_announced_only_after_it_cools() {
        let now = 10_000_000_u64;
        let candidates = vec![cand("todo:a")];
        let cooldown = 30 * 86_400;
        // Finished an hour ago: leave it alone, the fleet has other work.
        let fresh = board_with(&["todo:a"], now - 3_600, Some(now - 3_600));
        assert!(plan(&candidates, &fresh, now, cooldown).is_empty());
        // Finished two months ago: worth doing again.
        let stale = board_with(&["todo:a"], now - 60 * 86_400, Some(now - 60 * 86_400));
        assert_eq!(plan(&candidates, &stale, now, cooldown).len(), 1);
        // A failure cools the same way — an agent that gave up may just have
        // been the wrong agent, but retrying it immediately is a loop.
        let mut failed = board_with(&["todo:a"], now - 3_600, Some(now - 3_600));
        failed[0].done = Some((false, "could not build".into()));
        assert!(plan(&candidates, &failed, now, cooldown).is_empty());
        assert_eq!(plan(&candidates, &failed, now, 0).len(), 1, "with no cooldown, retry");
    }

    #[test]
    fn lcov_parses_into_per_file_counts() {
        let files = cov();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, "src/api.rs");
        assert_eq!((files[0].lines_found, files[0].lines_hit), (1000, 400));
        assert!((files[0].percent() - 40.0).abs() < f64::EPSILON);
        // A record with no counts still yields a file rather than vanishing.
        let partial = parse_lcov("SF:src/x.rs\nend_of_record\n");
        assert_eq!(partial.len(), 1);
        assert!(
            (partial[0].percent() - 100.0).abs() < f64::EPSILON,
            "nothing to cover is not 0%"
        );
        assert!(parse_lcov("").is_empty());
        assert!(parse_lcov("not lcov at all\n").is_empty());
        // An unterminated record is ignored — half a report is not a file.
        assert!(parse_lcov("SF:src/x.rs\nLF:10\n").is_empty());
    }

    #[test]
    fn the_coverage_source_ranks_by_shortfall_not_percentage() {
        let files = cov();
        let p = CoveragePolicy {
            audit_largest: 1,
            ..CoveragePolicy::default()
        };
        let c = coverage_candidates(&files, &p);
        let ids: Vec<&str> = c.iter().map(|c| c.id.as_str()).collect();
        // src/tiny.rs is 0% but 10 lines — below min_lines. src/good.rs is
        // already above target. A 1000-line file at 40% outranks a 300-line
        // one at 50%.
        assert_eq!(ids, vec!["cov:src/api.rs", "cov:src/mid.rs", "audit:src/api.rs"]);
        assert!(c[0].title.contains("40.0%"), "{}", c[0].title);
        assert!(c[0].title.contains("400 to reach 80%"), "{}", c[0].title);
        assert_eq!(worst_covered(&files, 80.0, 40, 1).len(), 1, "limit applies");
        assert_eq!(files[2].lines_to(80.0), 0, "a file at target asks for nothing");
    }

    #[test]
    fn coverage_ids_are_repo_relative_so_two_checkouts_agree() {
        let root = std::path::Path::new("/home/me/proj");
        assert_eq!(relative_to("/home/me/proj/src/api.rs", Some(root)), "src/api.rs");
        // A path outside the root is left alone rather than mangled.
        assert_eq!(relative_to("/usr/share/x.rs", Some(root)), "/usr/share/x.rs");
        assert_eq!(relative_to("src/api.rs", Some(root)), "src/api.rs");
        assert_eq!(
            relative_to("/home/me/proj/src/api.rs", None),
            "/home/me/proj/src/api.rs"
        );
        // The point: the same file in two checkouts yields ONE task id, so a
        // federated fleet doesn't announce and do the work twice.
        let other = std::path::Path::new("/srv/build/proj");
        assert_eq!(
            relative_to("/home/me/proj/src/api.rs", Some(root)),
            relative_to("/srv/build/proj/src/api.rs", Some(other))
        );
    }

    /// Announcing writes to the process-global board, so redirect it.
    fn with_board<T>(body: impl FnOnce() -> T) -> T {
        let _guard = crate::cli::team::BOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        crate::cli::team::set_blackboard_path(Some(dir.path().join("blackboard.jsonl")));
        let out = body();
        crate::cli::team::set_blackboard_path(None);
        out
    }

    #[test]
    fn a_sweep_announces_from_a_source_then_says_nothing_the_second_time() {
        with_board(|| {
            let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
            // A source is any command printing `id<TAB>title`.
            let src = "printf 'todo:a\\tclear the TODO in a\\ntodo:b\\tclear the TODO in b\\n'";
            assert_eq!(run(&argv(&["--from", src])), 0);
            let board = fold_tasks(&read_blackboard());
            let ids: Vec<&str> = board.iter().map(|t| t.id.as_str()).collect();
            assert_eq!(ids, vec!["todo:a", "todo:b"]);

            // The property that makes a timer safe: the same source, run
            // again, announces nothing rather than duplicating the board.
            assert_eq!(run(&argv(&["--from", src])), 0);
            assert_eq!(
                fold_tasks(&read_blackboard()).len(),
                2,
                "a repeat sweep duplicated work"
            );

            // --dry-run must not write, even for candidates that ARE new.
            let more = "printf 'todo:c\\tsomething new\\n'";
            assert_eq!(run(&argv(&["--from", more, "--dry-run"])), 0);
            assert_eq!(fold_tasks(&read_blackboard()).len(), 2, "dry run announced something");
        });
    }

    #[test]
    fn a_failing_source_is_reported_without_losing_the_healthy_ones() {
        with_board(|| {
            let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
            // One generator is broken, one works. The sweep must still put the
            // good work on the board — a fleet with one bad script is not a
            // fleet with nothing to do — while reporting the failure.
            let code = run(&argv(&[
                "--from",
                "echo broken >&2; exit 2",
                "--from",
                "printf 'ok:1\\tdo the thing\\n'",
            ]));
            assert_ne!(code, 0, "a failed source must be reported in the exit code");
            let board = fold_tasks(&read_blackboard());
            assert_eq!(board.len(), 1, "the healthy source's work was lost");
            assert_eq!(board[0].id, "ok:1");
        });
    }

    #[test]
    fn a_coverage_report_becomes_tasks_with_repo_relative_ids() {
        with_board(|| {
            let dir = tempfile::tempdir().unwrap();
            let lcov = dir.path().join("lcov.info");
            std::fs::write(&lcov, "SF:/build/checkout/src/api.rs\nLF:1000\nLH:100\nend_of_record\n").unwrap();
            let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
            assert_eq!(
                run(&argv(&[
                    "--lcov",
                    lcov.to_str().unwrap(),
                    "--root",
                    "/build/checkout",
                    "--audit-largest",
                    "0",
                ])),
                0
            );
            let board = fold_tasks(&read_blackboard());
            // Repo-relative, so the same file in another checkout is the SAME
            // task and two machines do not each do the work.
            assert_eq!(board.len(), 1);
            assert_eq!(board[0].id, "cov:src/api.rs");
            assert!(board[0].title.contains("10.0%"), "{}", board[0].title);
        });
    }

    #[test]
    fn args_are_validated() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(run(&argv(&["--limit", "abc"])), 2);
        assert_eq!(run(&argv(&["--target"])), 2);
        assert_eq!(run(&argv(&["--nope", "x"])), 2);
        assert_eq!(run(&argv(&["--help"])), 0);
        // No source at all is a usage error, not a silent no-op: the caller
        // asked for work and got none, and should be told why.
        assert_eq!(run(&argv(&[])), 2);
        assert_eq!(run(&argv(&["--from-file", "/nonexistent/tasks.tsv"])), 1);
    }
}
