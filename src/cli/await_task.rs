// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier wait` — block until named tasks finish, and say so with an
//! exit code.
//!
//! The alternative this replaces is `dispatch --wait`, which holds a
//! connection open and infers completion from a screen that stopped changing.
//! That is a guess (a thinking agent looks finished), it occupies the caller
//! for the whole duration, and it does not compose.
//!
//! Exit codes make it a shell primitive instead:
//!
//! ```text
//!   0  every named task finished ok
//!   1  at least one reported failure
//!   2  usage error
//!   3  still running when the timeout expired (or --timeout 0 and unfinished)
//!   4  no such task on the board
//! ```
//!
//! ```sh
//! tab-atelier wait cov:src/api.rs && echo "coverage done"
//!
//! # many in parallel — each is a cheap local file read, not a held connection
//! for t in $(tab-atelier tasks --ids); do tab-atelier wait "$t" & done; wait
//! ```
//!
//! `--timeout 0` turns it into a status check that returns immediately, so the
//! same verb covers "is it done?" and "tell me when it is".
//!
//! Waiting reads the blackboard file directly rather than calling the API: it
//! is the same data, it costs no connection, and a hundred parallel waiters
//! are a hundred cheap reads instead of a hundred open sockets.

use super::tasks::{TaskState, fold_tasks};
use super::team::read_blackboard;

/// Exit code for "not finished yet".
pub const EXIT_PENDING: i32 = 3;
/// Exit code for "no task by that id".
pub const EXIT_UNKNOWN: i32 = 4;

/// A parsed `wait` invocation.
#[derive(clap::Parser, Debug, Default, PartialEq, Eq)]
#[command(
    name = "tab-atelier wait",
    about = "Block until the named tasks finish",
    after_help = "Exit codes:\n  \
                  0  all finished ok        2  usage error        4  unknown task\n  \
                  1  one or more failed     3  still running at the timeout\n\n\
                  Cheap enough to run many at once: it reads the board, it does not hold\n\
                  a connection."
)]
pub struct WaitArgs {
    /// The tasks to wait for.
    #[arg(required = true)]
    pub ids: Vec<String>,
    /// Seconds to wait. `0` doesn't block at all — check and exit (a status
    /// probe). Omitted means wait indefinitely.
    #[arg(long = "timeout", short = 't', value_name = "SECONDS")]
    pub timeout_s: Option<u64>,
    /// Return as soon as ONE finishes, rather than all.
    #[arg(long)]
    pub any: bool,
    /// Say nothing; report through the exit code alone.
    #[arg(long, short = 'q')]
    pub quiet: bool,
}

/// # Errors
/// `Err(0)` on `--help`, `Err(2)` on a bad flag or no task ids.
pub fn parse_args(args: &[String]) -> Result<WaitArgs, i32> {
    super::parse::<WaitArgs>("tab-atelier wait", args)
}

/// What one poll of the board concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Everything asked for finished ok (or, with `--any`, one did).
    Done(Vec<String>),
    /// At least one task reported failure.
    Failed(Vec<String>),
    /// Some task isn't on the board at all — waiting forever on a typo is
    /// worse than saying so.
    Unknown(Vec<String>),
    /// Still in flight.
    Pending,
}

impl Verdict {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Done(_) => 0,
            Self::Failed(_) => 1,
            Self::Pending => EXIT_PENDING,
            Self::Unknown(_) => EXIT_UNKNOWN,
        }
    }
}

/// Decide the verdict for `ids` against a folded board. Pure, so the state
/// machine is testable without a board on disk or a sleeping loop.
#[must_use]
pub fn verdict(board: &[super::tasks::TaskView], ids: &[String], any: bool) -> Verdict {
    let mut unknown = Vec::new();
    let mut failed = Vec::new();
    let mut finished = Vec::new();
    for id in ids {
        match board.iter().find(|t| &t.id == id) {
            None => unknown.push(id.clone()),
            Some(t) => match t.state() {
                TaskState::Done => finished.push(id.clone()),
                TaskState::Failed => failed.push(id.clone()),
                _ => {}
            },
        }
    }
    // Unknown first: it is a caller mistake, and reporting "pending" for a
    // task that will never appear turns a typo into a hang.
    if !unknown.is_empty() {
        return Verdict::Unknown(unknown);
    }
    if !failed.is_empty() {
        return Verdict::Failed(failed);
    }
    if any {
        if finished.is_empty() {
            return Verdict::Pending;
        }
        return Verdict::Done(finished);
    }
    if finished.len() == ids.len() {
        Verdict::Done(finished)
    } else {
        Verdict::Pending
    }
}

/// How long to sleep between board reads. Short enough that a supervisor
/// reacts promptly, long enough that a hundred waiters are nothing.
const POLL: std::time::Duration = std::time::Duration::from_millis(500);

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let deadline = parsed
        .timeout_s
        .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
    loop {
        let board = fold_tasks(&read_blackboard());
        let v = verdict(&board, &parsed.ids, parsed.any);
        let terminal = !matches!(v, Verdict::Pending);
        let expired = deadline.is_some_and(|d| std::time::Instant::now() >= d);
        if terminal || expired {
            if !parsed.quiet {
                match &v {
                    Verdict::Done(ids) => println!("done: {}", ids.join(" ")),
                    Verdict::Failed(ids) => println!("failed: {}", ids.join(" ")),
                    Verdict::Unknown(ids) => eprintln!("wait: no such task: {}", ids.join(" ")),
                    Verdict::Pending => {
                        let open: Vec<&str> = parsed
                            .ids
                            .iter()
                            .filter(|id| {
                                board
                                    .iter()
                                    .find(|t| &&t.id == id)
                                    .is_none_or(|t| !matches!(t.state(), TaskState::Done | TaskState::Failed))
                            })
                            .map(String::as_str)
                            .collect();
                        println!("still running: {}", open.join(" "));
                    }
                }
            }
            return v.exit_code();
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tasks::TaskView;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn task(id: &str, done: Option<bool>) -> TaskView {
        TaskView {
            id: id.into(),
            title: String::new(),
            announced_by: None,
            home: Some("h".into()),
            announced_ts: 1,
            last_ts: 2,
            bids: Vec::new(),
            awarded_to: Some("agent-a".into()),
            done: done.map(|ok| (ok, String::new())),
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn args_parse_into_a_wait_or_an_exit_code() {
        assert_eq!(
            parse_args(&argv(&["t1", "t2", "--timeout", "30"])),
            Ok(WaitArgs {
                ids: ids(&["t1", "t2"]),
                timeout_s: Some(30),
                any: false,
                quiet: false,
            })
        );
        assert_eq!(parse_args(&argv(&["--any", "t1"])).map(|a| a.any), Ok(true));
        // Waiting on nothing is a mistake, not an instant success — exiting 0
        // there would tell a supervisor its work finished.
        assert_eq!(parse_args(&argv(&[])), Err(2));
        assert_eq!(parse_args(&argv(&["--timeout", "soon", "t1"])), Err(2));
        assert_eq!(parse_args(&argv(&["--nope"])), Err(2));
        assert_eq!(parse_args(&argv(&["--help"])), Err(0));
    }

    #[test]
    fn the_verdict_distinguishes_finished_failed_pending_and_unknown() {
        let board = vec![task("a", Some(true)), task("b", None), task("c", Some(false))];
        // All named tasks finished ok.
        assert_eq!(verdict(&board, &ids(&["a"]), false), Verdict::Done(ids(&["a"])));
        assert_eq!(verdict(&board, &ids(&["a"]), false).exit_code(), 0);
        // One still running: the whole wait is pending.
        assert_eq!(verdict(&board, &ids(&["a", "b"]), false), Verdict::Pending);
        assert_eq!(verdict(&board, &ids(&["a", "b"]), false).exit_code(), EXIT_PENDING);
        // …unless --any, which returns on the first finisher.
        assert_eq!(verdict(&board, &ids(&["a", "b"]), true), Verdict::Done(ids(&["a"])));
        assert_eq!(verdict(&board, &ids(&["b"]), true), Verdict::Pending);
        // A failure is not a success, and it is reported even alongside one.
        assert_eq!(verdict(&board, &ids(&["c"]), false).exit_code(), 1);
        assert_eq!(verdict(&board, &ids(&["a", "c"]), false), Verdict::Failed(ids(&["c"])));
        assert_eq!(
            verdict(&board, &ids(&["a", "c"]), true).exit_code(),
            1,
            "--any must not report success while a named task has failed"
        );
    }

    #[test]
    fn an_unknown_task_is_reported_rather_than_waited_on_forever() {
        // The typo case. Reporting "pending" here would hang a supervisor
        // until its timeout for a task that will never exist.
        let board = vec![task("a", Some(true))];
        assert_eq!(
            verdict(&board, &ids(&["typo"]), false),
            Verdict::Unknown(ids(&["typo"]))
        );
        assert_eq!(verdict(&board, &ids(&["a", "typo"]), false).exit_code(), EXIT_UNKNOWN);
        assert_eq!(verdict(&[], &ids(&["a"]), false).exit_code(), EXIT_UNKNOWN);
    }

    #[test]
    fn a_zero_timeout_is_a_status_probe_that_never_blocks() {
        // `--timeout 0` must return the current verdict immediately, so the
        // same verb answers "is it done?" and "tell me when it is".
        let started = std::time::Instant::now();
        let code = run(&argv(&["definitely-not-a-task", "--timeout", "0", "--quiet"]));
        assert_eq!(code, EXIT_UNKNOWN);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "it blocked");
    }
}
