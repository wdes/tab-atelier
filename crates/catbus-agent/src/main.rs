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

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(|| session.default_socket_path());

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

/// In-tab REPL: print a prompt, read a line, hand it to the agent,
/// print the answer, repeat. Ctrl-D exits.
///
/// Intentionally minimal — no readline, no history, no syntax
/// highlighting. Tab-atelier hosts the actual terminal so things
/// like cursor movement / line editing come from there. If a power
/// user wants `rlwrap catbus-agent`, that works too.
#[allow(clippy::too_many_lines)]
async fn run_repl(agent: Arc<agent::Agent>, cwd: &std::path::Path) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(tokio::io::stdin());

    print_banner(&mut stdout, &agent).await?;

    let mut line = String::new();
    loop {
        print_prompt(&mut stdout, &agent).await?;
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            // EOF (Ctrl-D)
            stdout.write_all(b"\n").await?;
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }

        // Erase the prompt line (move up one, clear to end-of-line) so
        // the terminal shows a clean slate before the reply or spinner.
        stdout.write_all(b"\x1b[1A\x1b[2K").await?;

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
                      /resume <id>       switch to a previous session in-place\n\n",
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
                let marker = if id == &current_id { " \x1b[32m(current)\x1b[0m" } else { "" };
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
        // can drive the spinner on stdout.
        let work = tokio::spawn(async move { agent_clone.run_user_prompt(prompt_owned).await });

        // Spinner loop: tick every 120 ms while `status` is set.
        let spinner_agent = Arc::clone(&agent);
        let mut frame: usize = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
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

        match work.await.expect("agent task panicked") {
            Ok(reply) => {
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
            Err(e) => {
                stdout
                    .write_all(format!("\n\x1b[31merror:\x1b[0m {e}\n\n").as_bytes())
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

async fn print_prompt(stdout: &mut tokio::io::Stdout, agent: &agent::Agent) -> std::io::Result<()> {
    let name = agent.session_name().await;
    if name.is_empty() {
        stdout.write_all(b"\x1b[36m>\x1b[0m ").await?;
    } else {
        stdout
            .write_all(format!("\x1b[36m{name}>\x1b[0m ").as_bytes())
            .await?;
    }
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
            stdout
                .write_all(format!("  \x1b[2m{part}\x1b[0m\n").as_bytes())
                .await?;
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
