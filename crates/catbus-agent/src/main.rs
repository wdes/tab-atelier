// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Catbus — the agent that drives one Claude session per process.
//!
//! Named after the many-windowed feline conveyance from *My Neighbor
//! Totoro*. Each `tab-atelier` tab can run one catbus instance, and
//! you talk to it through a per-session UNIX socket. Internally it
//! authenticates via Claude Code's OAuth credentials (so a Max
//! subscription works without an API key), persists the conversation
//! in the same JSONL shape Claude Code uses (so the existing
//! `/tabs/N/catbus/messages` endpoint Just Works), and runs a small
//! Read / Write / Edit / Bash tool loop.

#![allow(clippy::module_name_repetitions)]

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reedline::{
    FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, Signal,
};
use tokio::io::AsyncWriteExt;

mod agent;
mod auth;
mod session;
mod socket;
mod tools;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Claude agent for tab-atelier. Many tabs, many windows.",
    long_about = None,
)]
struct Args {
    /// Working directory the session runs in. Defaults to $PWD.
    /// Mirrors Claude Code's `cwd` field on every message.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Resume an existing session by id. Without this flag the
    /// newest session in the working directory is auto-resumed
    /// (use --new-session to override).
    #[arg(long)]
    resume: Option<String>,

    /// Force a brand-new session even when a previous transcript
    /// exists in this cwd. Default behaviour is to resume.
    #[arg(long)]
    new_session: bool,

    /// Set or update the human-readable name for this session.
    #[arg(long)]
    name: Option<String>,

    /// Path to the UNIX socket the agent listens on. Defaults to
    /// `~/.claude/projects/{escaped-cwd}/{session-id}.sock`, so any
    /// external client that knows the session id can connect.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Print the resolved socket path and exit. Handy for the
    /// tab-atelier API server when it needs to forward POSTs.
    #[arg(long)]
    print_socket: bool,

    /// Skip the in-tab REPL and only listen on the UNIX socket.
    /// Useful when catbus-agent is launched as a background service
    /// rather than from a tab the user is staring at.
    #[arg(long)]
    no_tui: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // REPL mode shares the tab with stdout, so even "to stderr" logs
    // print in the same window. Quiet the floor to `warn` unless the
    // user explicitly set RUST_LOG — the socket-only path still gets
    // info-level chatter because nobody's reading those tabs.
    let default_level = if args.no_tui { "info" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .target(env_logger::Target::Stderr)
        .init();

    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    // Auth must succeed *before* we open the socket — no point
    // accepting prompts we can't service.
    let auth = auth::load()?;
    let session = session::open(&cwd, args.resume.as_deref(), args.new_session)?;

    // Apply --name if provided (also works as a rename on resume).
    if let Some(ref name) = args.name {
        session.rename(name)?;
    }

    let socket_path = args.socket.clone().unwrap_or_else(|| session.default_socket_path());

    if args.print_socket {
        println!("{}", socket_path.display());
        return Ok(());
    }

    log::info!("session {} ready at {}", session.id, socket_path.display());
    let agent = Arc::new(agent::Agent::new(auth, session));

    let socket_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        let path = socket_path.clone();
        async move { socket::serve(agent, path).await }
    });

    if args.no_tui {
        // Headless: just block on the socket task.
        socket_task.await??;
    } else {
        run_repl(Arc::clone(&agent), &cwd).await?;
        // REPL exit (Ctrl-D) brings the whole process down so the
        // tab the user closed feels "closed". Aborting the socket
        // task removes its file in Drop on a best-effort basis.
        socket_task.abort();
    }
    Ok(())
}

/// Spinner frames — simple ASCII so any font renders them.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// reedline `Prompt` impl that renders the session-name prefix in cyan.
/// The name is snapshotted once per `read_line()` call so reedline can
/// keep calling these methods without touching the async lock.
struct CatbusPrompt {
    name: String,
}

impl Prompt for CatbusPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        if self.name.is_empty() {
            Cow::Borrowed("\x1b[36m>\x1b[0m ")
        } else {
            Cow::Owned(format!("\x1b[36m{}>\x1b[0m ", self.name))
        }
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("\x1b[36m·\x1b[0m ")
    }
    fn render_prompt_history_search_indicator(&self, s: PromptHistorySearch) -> Cow<'_, str> {
        let prefix = match s.status {
            PromptHistorySearchStatus::Passing => "search",
            PromptHistorySearchStatus::Failing => "search failed",
        };
        Cow::Owned(format!("({prefix}: {}) ", s.term))
    }
}

/// History file lives under `XDG_STATE_HOME` (or `~/.local/state`) so it
/// survives across sessions but stays out of the user's `$HOME`.
fn history_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("catbus-agent").join("history.txt")
}

/// Build a reedline editor with file-backed history. Caps at 5000
/// entries so the file doesn't grow without bound.
fn make_editor() -> Reedline {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let history = FileBackedHistory::with_file(5000, path).map(Box::new).ok();
    let mut editor = Reedline::create();
    if let Some(h) = history {
        editor = editor.with_history(h);
    }
    editor
}

/// In-tab REPL: print a prompt, read a line, hand it to the agent,
/// print the answer, repeat.
///
/// Line editing comes from `reedline` (history, cursor movement,
/// Ctrl-R search). Ctrl-C while typing clears the buffer and re-prompts;
/// Ctrl-C while the agent is working cancels the in-flight request via
/// `agent.cancel_current()`. Ctrl-D exits.
#[allow(clippy::too_many_lines)]
async fn run_repl(agent: Arc<agent::Agent>, cwd: &std::path::Path) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();

    print_banner(&mut stdout, &agent).await?;

    // reedline is sync — we move it into a `spawn_blocking` for each
    // read_line and pull it back so the main task can keep driving the
    // tokio runtime (signals, agent work, etc.).
    let mut editor_slot: Option<Reedline> = Some(make_editor());

    loop {
        let name = agent.session_name().await;
        let prompt = CatbusPrompt { name };
        let mut editor = editor_slot.take().expect("editor present");
        let (sig, returned) = tokio::task::spawn_blocking(move || {
            let res = editor.read_line(&prompt);
            (res, editor)
        })
        .await
        .expect("reedline task panicked");
        editor_slot = Some(returned);

        let line = match sig? {
            Signal::Success(line) => line,
            Signal::CtrlC => {
                // Just clear the line; don't kill the process.
                continue;
            }
            Signal::CtrlD => {
                stdout.write_all(b"\n").await?;
                break;
            }
        };

        let prompt = line.trim().trim_matches('`');
        if prompt.is_empty() {
            continue;
        }

        // Slash commands are interpreted locally; everything else
        // becomes a user turn for the model.
        if prompt == "/help" {
            stdout
                .write_all(
                    b"slash commands:\n  \
                      /help              show this list\n  \
                      /plan              enable plan-mode (write/edit/bash refuse)\n  \
                      /noplan            disable plan-mode\n  \
                      /rename <name>     rename the current session\n  \
                      /resume            list previous sessions in this cwd\n  \
                      /resume <id>       switch to a previous session in-place\n  \
                      /deb               build the .deb and print its path\n\n",
                )
                .await?;
            continue;
        }
        if prompt == "/plan" {
            agent.set_plan_mode(true);
            stdout.write_all(b"plan-mode = true\n").await?;
            continue;
        }
        if prompt == "/noplan" {
            agent.set_plan_mode(false);
            stdout.write_all(b"plan-mode = false\n").await?;
            continue;
        }
        if prompt == "/deb" {
            stdout.write_all(b"\x1b[36mbuilding .deb...\x1b[0m\n").await?;
            stdout.flush().await?;
            let out = tokio::process::Command::new("cargo")
                .args(["deb", "--no-build"])
                .current_dir(cwd)
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    // cargo-deb prints the .deb path as the last non-empty
                    // line of stdout.
                    let text = String::from_utf8_lossy(&o.stdout);
                    let path = text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("(no output)");
                    let path = path.trim().trim_matches('`');
                    stdout.write_all(format!("\x1b[1m{path}\x1b[0m\n").as_bytes()).await?;
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    stdout
                        .write_all(format!("\x1b[31merror:\x1b[0m cargo-deb failed\n{stderr}").as_bytes())
                        .await?;
                }
                Err(e) => {
                    stdout
                        .write_all(format!("\x1b[31merror:\x1b[0m could not run cargo-deb: {e}\n").as_bytes())
                        .await?;
                }
            }
            continue;
        }
        if let Some(new_name) = prompt.strip_prefix("/rename ") {
            let new_name = new_name.trim();
            if new_name.is_empty() {
                stdout.write_all(b"usage: /rename <name>\n").await?;
                continue;
            }
            match agent.rename_session(new_name).await {
                Ok(()) => {
                    stdout
                        .write_all(format!("session renamed to \x1b[1m{new_name}\x1b[0m\n").as_bytes())
                        .await?;
                }
                Err(e) => {
                    stdout
                        .write_all(format!("\x1b[31merror:\x1b[0m {e}\n").as_bytes())
                        .await?;
                }
            }
            continue;
        }
        if prompt == "/rename" {
            stdout.write_all(b"usage: /rename <name>\n").await?;
            continue;
        }
        if let Some(target_id) = prompt.strip_prefix("/resume ") {
            let target_id = target_id.trim();
            if target_id.is_empty() {
                stdout.write_all(b"usage: /resume <session-id>\n").await?;
                continue;
            }
            match session::open(cwd, Some(target_id), false) {
                Ok(new_session) => {
                    let new_id = new_session.id.clone();
                    let new_name = new_session.session_name();
                    match agent.swap_session(new_session).await {
                        Ok(()) => {
                            let label = if new_name.is_empty() {
                                format!("\x1b[2m{new_id}\x1b[0m")
                            } else {
                                format!("\x1b[1m{new_name}\x1b[0m  \x1b[2m{new_id}\x1b[0m")
                            };
                            stdout
                                .write_all(format!("switched to session {label}\n").as_bytes())
                                .await?;
                            let path = agent.transcript_path().await;
                            print_exchanges(&mut stdout, &path).await?;
                            stdout.write_all(b"\n").await?;
                        }
                        Err(e) => {
                            stdout
                                .write_all(format!("\x1b[31merror:\x1b[0m swap failed: {e}\n").as_bytes())
                                .await?;
                        }
                    }
                }
                Err(e) => {
                    stdout
                        .write_all(format!("\x1b[31merror:\x1b[0m could not open session: {e}\n").as_bytes())
                        .await?;
                }
            }
            continue;
        }
        if prompt == "/resume" {
            let sessions = session::list_sessions(cwd);
            if sessions.is_empty() {
                stdout.write_all(b"no previous sessions in this cwd.\n\n").await?;
                continue;
            }
            let current_id = agent.session_id().await;
            stdout
                .write_all(format!("{} session(s) for {}:\n", sessions.len(), cwd.display()).as_bytes())
                .await?;
            let now = std::time::SystemTime::now();
            for (id, name, ts) in &sessions {
                let age = now
                    .duration_since(*ts)
                    .map_or_else(|_| "in the future".to_string(), |d| humanise_age(d.as_secs()));
                let marker = if id == &current_id {
                    " \x1b[32m(current)\x1b[0m"
                } else {
                    ""
                };
                let label = if name.is_empty() {
                    format!("\x1b[2m{id}\x1b[0m")
                } else {
                    format!("\x1b[1m{name}\x1b[0m  \x1b[2m{id}\x1b[0m")
                };
                stdout
                    .write_all(format!("  {label}  {age}{marker}\n").as_bytes())
                    .await?;
            }
            stdout
                .write_all(b"\nto switch in-place: /resume <session-id>\n\n")
                .await?;
            continue;
        }

        // Regular prompt — run through the agent, show spinner while working.
        let agent_clone = Arc::clone(&agent);
        let prompt_owned = prompt.to_string();

        // Spawn the agent work on a concurrent task so the main task
        // can drive the spinner + a SIGINT watcher.
        let work = tokio::spawn(async move { agent_clone.run_user_prompt(prompt_owned).await });

        // Reedline puts the terminal back into canonical mode when
        // read_line returns, so Ctrl+C now reaches us as a real SIGINT.
        // Race it against the spinner loop: first to fire wins.
        let spinner_agent = Arc::clone(&agent);
        let mut frame: usize = 0;
        let mut interrupted = false;
        loop {
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(120)) => {}
                res = tokio::signal::ctrl_c() => {
                    // Best-effort — if the signal handler can't install
                    // (rare), just fall through and let the spinner finish.
                    if res.is_ok() {
                        agent.cancel_current();
                        interrupted = true;
                        stdout.write_all(b"\r\x1b[K\x1b[33minterrupted\x1b[0m, cancelling...\n").await?;
                        stdout.flush().await?;
                        break;
                    }
                }
            }
            let current_status = spinner_agent.status.lock().expect("status mutex").clone();
            let Some(label) = current_status else {
                // Erase the spinner line before printing the reply.
                stdout.write_all(b"\r\x1b[K").await?;
                stdout.flush().await?;
                break;
            };
            let spinner_char = SPINNER[frame % SPINNER.len()];
            stdout
                .write_all(format!("\r\x1b[36m{spinner_char}\x1b[0m {label}").as_bytes())
                .await?;
            stdout.flush().await?;
            frame += 1;
        }

        // If we requested cancel, wait briefly for the work task to
        // notice — but don't hang the REPL on a stuck tool.
        let result = if interrupted {
            tokio::time::timeout(std::time::Duration::from_secs(5), work).await
        } else {
            Ok(work.await)
        };

        match result {
            Ok(Ok(Ok(reply))) => {
                // Persist token usage sidecar so tab-atelier can pick it up.
                let session = agent.active_session().await;
                let _ = session.save_tokens(
                    agent.tokens_in.load(std::sync::atomic::Ordering::Relaxed),
                    agent.tokens_out.load(std::sync::atomic::Ordering::Relaxed),
                );
                stdout.write_all(b"\n").await?;
                stdout.write_all(reply.as_bytes()).await?;
                stdout.write_all(b"\n\n").await?;
            }
            Ok(Ok(Err(e))) => {
                stdout
                    .write_all(format!("\n\x1b[31merror:\x1b[0m {e}\n\n").as_bytes())
                    .await?;
            }
            Ok(Err(join)) => {
                stdout
                    .write_all(format!("\n\x1b[31merror:\x1b[0m agent task: {join}\n\n").as_bytes())
                    .await?;
            }
            Err(_timeout) => {
                stdout
                    .write_all(b"\n\x1b[31merror:\x1b[0m cancel timed out; abandoning task\n\n")
                    .await?;
            }
        }
    }
    Ok(())
}

async fn print_banner(stdout: &mut tokio::io::Stdout, agent: &agent::Agent) -> std::io::Result<()> {
    stdout
        .write_all(b"\x1b[1m\xf0\x9f\x90\x88\xef\xb8\x8f\xf0\x9f\x9a\x8c Catbus\x1b[0m \xe2\x80\x94 type a prompt, /help for commands, Ctrl-D to exit.\n")
        .await?;
    let id = agent.session_id().await;
    let name = agent.session_name().await;
    if name.is_empty() {
        stdout
            .write_all(format!("session \x1b[2m{id}\x1b[0m\n").as_bytes())
            .await?;
    } else {
        stdout
            .write_all(format!("session \x1b[1m{name}\x1b[0m  \x1b[2m{id}\x1b[0m\n").as_bytes())
            .await?;
    }
    let path = agent.transcript_path().await;
    print_exchanges(stdout, &path).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await
}

/// Print the last 3 exchanges from `path` as a compact recap.
/// Each exchange is: dim user prompt, then assistant reply (possibly
/// truncated). A separator line precedes the block when exchanges exist.
async fn print_exchanges(stdout: &mut tokio::io::Stdout, path: &std::path::Path) -> std::io::Result<()> {
    let exchanges = session::last_exchanges(path, 3);
    if exchanges.is_empty() {
        return Ok(());
    }
    stdout.write_all(b"\x1b[2m--- recent exchanges ---\x1b[0m\n").await?;
    for ex in &exchanges {
        // User line: cyan >, dim text, truncated at 120 chars.
        let user_preview: String = ex.user_text.lines().next().unwrap_or("").chars().take(120).collect();
        stdout
            .write_all(format!("\x1b[36m>\x1b[0m \x1b[2m{user_preview}\x1b[0m\n").as_bytes())
            .await?;
        // Assistant reply: indent, wrap long lines with a continuation marker.
        for part in ex.assistant_text.lines().take(4) {
            stdout.write_all(format!("  \x1b[2m{part}\x1b[0m\n").as_bytes()).await?;
        }
        if ex.assistant_text.lines().count() > 4 {
            stdout.write_all(b"  \x1b[2m...\x1b[0m\n").await?;
        }
    }
    Ok(())
}

fn humanise_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
