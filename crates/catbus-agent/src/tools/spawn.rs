// SPDX-License-Identifier: MPL-2.0

//! Start a second catbus-agent for one task, take its reply, and reap it.
//!
//! `Delegate` prompts an agent that already exists; this creates one. Before it,
//! an agent could discover peers (`ListAgents`) and talk to them (`Delegate`) but
//! could not mint a worker — so any "spawn a helper" workflow had to be driven
//! from outside the agent entirely, by a shell script the agent cannot see. This
//! is the missing primitive, and the counterpart to the fleet playbook's
//! "spawn workers" step.
//!
//! The child is *this* binary (`std::env::current_exe()`), so parent and child
//! are never different builds. The parent picks the socket path, so it knows it
//! without discovery; the child still mints its own session id, and nothing here
//! invents one.
//!
//! Ephemeral on purpose: the child is killed before this returns. That is what
//! makes it leak-free without a supervisor — a child left behind needs a reaper,
//! and an unreaped child is a zombie. Reuse across tasks is what `Delegate` is
//! for, against a peer that is still alive.
//!
//! Every child is a full agent session and so costs money. Three things bound
//! that: [`MAX_DEPTH`], passed down as the `CATBUS_SPAWN_DEPTH` env var so a child
//! cannot spawn grandchildren forever; [`MAX_CONCURRENT`]; and the timeout, which
//! shares `Delegate`'s default and ceiling.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::delegate;

/// How long the child gets to bind its socket and answer the handshake.
const READY_DEADLINE: Duration = Duration::from_secs(15);
/// How long a signalled child gets to actually die before it is reported as
/// abandoned.
const REAP_DEADLINE: Duration = Duration::from_secs(5);
/// Children alive at once, per process. Each one is a paid agent session.
const MAX_CONCURRENT: usize = 3;
/// How many generations of spawn may exist, counting this one. A child is told
/// its own depth through the environment, so the ceiling holds across processes
/// rather than only within one.
const MAX_DEPTH: u8 = 2;
/// Tool set for a child that did not ask for one. The smallest set that can
/// still finish a file task — deliberately *not* the parent's set, because a
/// worker that inherits `Bash` and `Spawn` is how a fleet turns into a fork bomb.
const DEFAULT_TOOLS: &str = "minimal";
/// How much of a failed child's log to quote back.
const LOG_TAIL: usize = 2048;
/// Children currently alive. Static because the limit is per process, and the
/// agent lives for one process.
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let task = input
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing task".to_string())?;
    let work_dir = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map_or_else(|| cwd.to_path_buf(), |p| super::resolve(cwd, p));
    let tools = input
        .get("tools_config")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_TOOLS);
    let timeout = delegate::requested_timeout(input);

    // Refuse before starting anything, so a refusal costs nothing: no process,
    // no tokens, no socket to clean up.
    if !work_dir.is_dir() {
        return Err(format!("working directory {} does not exist", work_dir.display()));
    }
    let depth = depth_from_env();
    if depth >= MAX_DEPTH {
        return Err(format!(
            "spawn refused: this agent is already {depth} level(s) deep and the cap is {MAX_DEPTH}. \
             Delegate to an existing peer instead of starting another generation."
        ));
    }
    // Reject a bad tool set here rather than watching the child die of it: the
    // message names the set, where the child would only report an exit code.
    if let Err(why) = super::ToolSet::load(Some(Path::new(tools))) {
        return Err(format!("spawn refused: tools_config `{tools}`: {why}"));
    }
    let _slot = LiveSlot::claim()?;

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate my own binary: {e}"))?;
    let socket = socket_path()?;
    let log_path = socket.with_extension("log");
    let log =
        std::fs::File::create(&log_path).map_err(|e| format!("cannot write child log {}: {e}", log_path.display()))?;
    let err_log = log.try_clone().map_err(|e| format!("cannot clone child log: {e}"))?;

    let started = Instant::now();
    let mut command = Command::new(&exe);
    command
        .arg("--cwd")
        .arg(&work_dir)
        .arg("--new-session")
        .arg("--no-tui")
        .arg("--socket")
        .arg(&socket)
        .arg("--tools-config")
        .arg(tools)
        // Answer one prompt, then exit. This is what makes the teardown
        // structural rather than defensive: if this agent is killed mid-call,
        // nothing here runs, so a child that waited to be reaped would stay
        // alive forever, holding a session open. A child that leaves on its own
        // cannot be orphaned, whatever happens to its parent.
        .arg("--once")
        .env("CATBUS_SPAWN_DEPTH", (depth + 1).to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err_log))
        // `Child` does not kill on drop, so without this every failure path
        // below would leak a running agent. `bash.rs` had the same hole.
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;

    // The child exists from here, so every path must reap it.
    let outcome = run_child(&mut child, &socket, task, timeout).await;
    let reap = kill_and_reap(&mut child).await;
    let elapsed = started.elapsed();

    // The socket is this call's own scratch file, and the child may not have
    // unlinked it. The log is worth keeping only when something went wrong.
    let _ = std::fs::remove_file(&socket);

    match outcome {
        Ok(reply) => {
            let _ = std::fs::remove_file(&log_path);
            Ok(format!(
                "{reply}\n\n[sub-agent finished in {}s from a fresh session with the `{tools}` tools{}]",
                elapsed.as_secs(),
                reap.footer()
            ))
        }
        Err(why) => {
            let tail = log_tail(&log_path, LOG_TAIL);
            if tail.is_empty() {
                let _ = std::fs::remove_file(&log_path);
                Err(why)
            } else {
                Err(format!("{why}\n--- child log ({}) ---{tail}", log_path.display()))
            }
        }
    }
}

/// Wait for the child to be up, then hand it the task — over one connection.
///
/// One connection, because the child runs with `--once` and therefore answers
/// exactly one: a separate readiness probe would consume it and leave the task
/// with nothing to talk to. See [`delegate::rpc_starting`], which exists for that
/// reason.
async fn run_child(
    child: &mut tokio::process::Child,
    socket: &Path,
    task: &str,
    timeout: Duration,
) -> Result<String, String> {
    delegate::rpc_starting(socket, task, READY_DEADLINE, timeout)
        .await
        .map_err(|why| {
            // A child that died at startup said why in its log, and "not ready" is
            // the same message for a bad flag as for a crash. The exit status is
            // the cheap half of telling them apart; the caller appends the log.
            match child.try_wait() {
                Ok(Some(status)) => format!("{why}; the child exited with {status}"),
                _ => why,
            }
        })
}

/// What became of the child.
///
/// Three cases rather than a `bool`, because they need different words in the
/// reply and only one of them is a problem. Collapsing `Abandoned` into a plain
/// "stopped" is the kind of report that makes a leak invisible: the operator is
/// told the child is gone when it is not.
enum Reap {
    /// It answered and exited by itself — the normal ending with `--once`.
    Exited,
    /// It was still running, so it was signalled and waited for.
    Stopped,
    /// It would not die. Something is wrong with it; the operator is told, and
    /// told to look, rather than left believing the cleanup worked.
    Abandoned,
}

impl Reap {
    /// The clause appended to the reply footer. Empty when the ending was
    /// ordinary, so the common case says nothing extra.
    const fn footer(&self) -> &'static str {
        match self {
            Self::Exited => "",
            Self::Stopped => "; it was still running and has been stopped",
            Self::Abandoned => {
                "; IT DID NOT STOP — it may still be running, check with `ps` for a catbus-agent on this cwd"
            }
        }
    }
}

/// Terminate the child and reap it, so it cannot become a zombie.
///
/// The wait is bounded. An unbounded `wait()` here would hang the parent — and
/// the parent is mid-turn inside a tool call, so the operator would see a frozen
/// agent rather than a failed one. This was a real hang, found by testing the
/// leak path rather than by reading the code.
async fn kill_and_reap(child: &mut tokio::process::Child) -> Reap {
    // `try_wait` reaps an already-exited child as a side effect, so this is also
    // the "was it already done" check.
    if !matches!(child.try_wait(), Ok(None)) {
        return Reap::Exited;
    }
    let _ = child.start_kill();
    // `--once` means the child is usually on its way out already, and a process
    // that has been signalled still needs to be waited for or it stays a zombie.
    if tokio::time::timeout(REAP_DEADLINE, child.wait()).await.is_ok() {
        return Reap::Stopped;
    }
    match child.try_wait() {
        // Died just as the deadline passed: the wait is served, nothing is left.
        Ok(Some(_)) => Reap::Stopped,
        _ => Reap::Abandoned,
    }
}

/// A claimed slot in [`MAX_CONCURRENT`], released on drop so no early return can
/// leak one.
#[must_use]
struct LiveSlot;

impl LiveSlot {
    fn claim() -> Result<Self, String> {
        let already = LIVE.fetch_add(1, Ordering::SeqCst);
        if already >= MAX_CONCURRENT {
            LIVE.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "spawn refused: {already} sub-agents already running (cap {MAX_CONCURRENT}). \
                 Wait for one to finish, or delegate to a peer that already exists."
            ));
        }
        Ok(Self)
    }
}

impl Drop for LiveSlot {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How deep this agent already is. The parent sets this for its children, so a
/// process that was started by hand reads 0.
fn depth_from_env() -> u8 {
    std::env::var("CATBUS_SPAWN_DEPTH")
        .ok()
        .as_deref()
        .and_then(parse_depth)
        .unwrap_or(0)
}

/// Parse an inherited depth value, clamping anything unexpected up to the
/// ceiling.
///
/// Clamped rather than passed through, and unparseable values become `None`, both
/// on purpose: this variable is inherited from whatever process started this one,
/// so it is not trustworthy, and the two directions are not symmetrical. Reading
/// it too strictly would let a typo *lower* a real depth and permit spawning that
/// should have been refused; clamping means the worst a hostile value can do is
/// prohibit.
fn parse_depth(value: &str) -> Option<u8> {
    value.trim().parse::<u8>().ok().map(|d| d.min(MAX_DEPTH))
}

/// A socket path no other spawn in this process is using.
///
/// `sun_path` is 108 bytes on Linux, so this stays in the temp dir rather than
/// anywhere derived from the working directory. A leftover file from a crash
/// would make the child refuse to bind, so it is cleared first.
fn socket_path() -> Result<PathBuf, String> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("catbus-sub-{}-{seq}.sock", std::process::id()));
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot clear {}: {e}", path.display())),
    }
    Ok(path)
}

/// The end of the child's log, to explain a failure. Empty when there is none.
fn log_tail(path: &Path, limit: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    // Cut on a character boundary: the tail is byte-counted, and a log line is
    // not obliged to be ASCII.
    let start = text.len().saturating_sub(limit);
    let start = (start..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    format!("\n{}", &text[start..])
}

/// The tool's schema, defined here so it cannot drift from the defaults above.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Spawn",
        "description": "Start a fresh catbus-agent, give it one task, and get its reply. The \
                        sub-agent shares no context with this session; it sees only `task`. It \
                        answers once and exits, so ask for everything you want in one task. Every \
                        sub-agent is a separate agent session and costs money — prefer `Delegate` \
                        to an agent that already exists, and use this when there is none. Defaults \
                        to the `minimal` tool set (no shell).",
        "input_schema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The whole task for the sub-agent: what to do, and what to report back. It cannot see this conversation."
                },
                "cwd": {
                    "type": "string",
                    "description": "Directory the sub-agent works in. Defaults to this session's. Relative paths resolve against it."
                },
                "tools_config": {
                    "type": "string",
                    "description": "Tool set for the sub-agent: the keyword `minimal` (Read, Write, FileTree) or a path to a tools config. Defaults to `minimal`; widen it only when the task needs it."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Override the 5-minute default. Capped at 1800."
                }
            },
            "required": ["task"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_depth_ceiling_reads_and_clamps_what_it_inherits() {
        // This is what makes the ceiling hold *across* processes: a child inherits
        // the parent's depth plus one, so the cap cannot be escaped by starting a
        // new agent. A parse that let the value through unchecked would allow
        // unlimited generations, which is the fork-bomb case.
        assert_eq!(parse_depth("0"), Some(0));
        assert_eq!(parse_depth("1"), Some(1));
        assert_eq!(parse_depth(" 2 "), Some(2));
        // A value past the ceiling clamps *up* to it, so a forged depth prohibits
        // spawning rather than permitting it. This is the direction that matters:
        // the variable is inherited, so it is not trustworthy, and the permissive
        // mistake here is unbounded agents.
        assert_eq!(parse_depth("99"), Some(MAX_DEPTH));
        // Unparseable is "not set", which means a hand-started agent at depth 0 —
        // the caller decides, and `depth_from_env` is what applies the 0.
        assert_eq!(parse_depth(""), None);
        assert_eq!(parse_depth("two"), None);
        assert_eq!(parse_depth("-1"), None);
    }

    #[test]
    fn a_leftover_socket_is_cleared_and_the_path_stays_short() {
        let path = socket_path().expect("a temp path");
        // `sun_path` is 108 bytes on Linux: a longer path fails to bind, and it
        // would fail in the child, where the message is only an exit code.
        assert!(path.as_os_str().len() < 100, "{} is too long to bind", path.display());
        // Two calls disagree, or concurrent spawns would fight over one socket.
        assert_ne!(path, socket_path().expect("a second temp path"));
    }

    #[test]
    fn the_third_claim_is_refused_and_a_drop_gives_the_slot_back() {
        // Reset first: the counter is process-wide, and another test in this
        // binary may have held a slot.
        LIVE.store(0, Ordering::SeqCst);
        let held: Vec<LiveSlot> = (0..MAX_CONCURRENT)
            .map(|_| LiveSlot::claim().expect("under cap"))
            .collect();
        assert!(
            LiveSlot::claim().is_err(),
            "a fourth concurrent sub-agent must be refused"
        );
        // Dropping one must release exactly one slot: a leaked slot would make
        // the cap decay until spawn stopped working, which reads as a hang.
        drop(held);
        assert!(LiveSlot::claim().is_ok(), "slots must be released on drop");
        LIVE.store(0, Ordering::SeqCst);
    }

    #[test]
    fn the_spec_names_the_tool_and_requires_a_task() {
        let spec = spec();
        assert_eq!(spec["name"], "Spawn");
        assert_eq!(spec["input_schema"]["required"][0], "task");
        // The description has to warn about cost: the model is the only thing
        // choosing whether to spend a whole session, and it cannot see the
        // operator's bill.
        let described = spec["description"].as_str().unwrap_or_default();
        assert!(described.contains("costs money"), "the cost warning is load-bearing");
    }

    #[test]
    fn a_log_tail_is_bounded_and_survives_multibyte_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("child.log");
        // Multi-byte on purpose: the slice is counted in bytes, so cutting
        // without a boundary check would panic here.
        std::fs::write(&path, "é".repeat(4000)).unwrap();
        let tail = log_tail(&path, 100);
        assert!(tail.len() < 200, "the tail must be bounded, got {}", tail.len());

        // No file, and an empty file, are both "nothing to say" rather than an
        // error: the caller appends this to a message that already explains.
        assert!(log_tail(&dir.path().join("absent.log"), 100).is_empty());
        std::fs::write(&path, "   \n").unwrap();
        assert!(log_tail(&path, 100).is_empty());
    }
}
