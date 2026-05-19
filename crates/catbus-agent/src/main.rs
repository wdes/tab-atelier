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
    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(|| session.default_socket_path());

    if args.print_socket {
        println!("{}", socket_path.display());
        return Ok(());
    }

    log::info!("session {} ready at {}", session.id, socket_path.display());
    let session_id_for_banner = session.id.clone();
    let cwd_for_banner = session.cwd.clone();
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
        run_repl(Arc::clone(&agent), &session_id_for_banner, &cwd_for_banner).await?;
        // REPL exit (Ctrl-D) brings the whole process down so the
        // tab the user closed feels "closed". Aborting the socket
        // task removes its file in Drop on a best-effort basis.
        socket_task.abort();
    }
    Ok(())
}

/// In-tab REPL: print a prompt, read a line, hand it to the agent,
/// print the answer, repeat. Ctrl-D exits.
///
/// Intentionally minimal — no readline, no history, no syntax
/// highlighting. Tab-atelier hosts the actual terminal so things
/// like cursor movement / line editing come from there. If a power
/// user wants `rlwrap catbus-agent`, that works too.
async fn run_repl(
    agent: Arc<agent::Agent>,
    session_id: &str,
    cwd: &std::path::Path,
) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(tokio::io::stdin());
    stdout
        .write_all(b"\x1b[1m\xf0\x9f\x90\x88\xef\xb8\x8f\xf0\x9f\x9a\x8c Catbus\x1b[0m \xe2\x80\x94 type a prompt, /help for commands, Ctrl-D to exit.\n")
        .await?;
    stdout
        .write_all(format!("session \x1b[2m{session_id}\x1b[0m\n\n").as_bytes())
        .await?;
    let cwd = cwd.to_path_buf();
    let mut line = String::new();
    loop {
        stdout.write_all(b"\x1b[36m>\x1b[0m ").await?;
        stdout.flush().await?;
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
        // Slash commands are interpreted locally; everything else
        // becomes a user turn for the model.
        if prompt == "/help" {
            stdout
                .write_all(
                    b"slash commands:\n  \
                      /help              show this list\n  \
                      /plan              enable plan-mode (write/edit/bash refuse)\n  \
                      /noplan            disable plan-mode\n  \
                      /resume            list previous sessions in this cwd\n\n\
                      to switch to a previous session, exit and run:\n  \
                      catbus-agent --resume <session-id>\n\n",
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
        if prompt == "/resume" {
            let sessions = session::list_sessions(&cwd);
            if sessions.is_empty() {
                stdout
                    .write_all(b"no previous sessions in this cwd.\n\n")
                    .await?;
                continue;
            }
            // Format times as relative-to-now for readability —
            // the absolute mtime isn't useful to a human.
            stdout
                .write_all(format!("{} session(s) for {}:\n", sessions.len(), cwd.display()).as_bytes())
                .await?;
            let now = std::time::SystemTime::now();
            for (id, ts) in &sessions {
                let age = now
                    .duration_since(*ts)
                    .map_or_else(|_| "in the future".to_string(), |d| humanise_age(d.as_secs()));
                let marker = if id == session_id { " (current)" } else { "" };
                stdout
                    .write_all(format!("  {id}  {age}{marker}\n").as_bytes())
                    .await?;
            }
            stdout
                .write_all(b"\nto switch, exit and run: catbus-agent --resume <session-id>\n\n")
                .await?;
            continue;
        }
        match agent.run_user_prompt(prompt.to_string()).await {
            Ok(reply) => {
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
