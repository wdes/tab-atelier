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

    /// Resume an existing session by id. Without this flag a fresh
    /// session is started and a new UUID is allocated.
    #[arg(long)]
    resume: Option<String>,

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
    // When the user runs this in a tab they want to see, redirect
    // logger output to stderr only — the stdout stream belongs to
    // the REPL.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
    let args = Args::parse();

    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    // Auth must succeed *before* we open the socket — no point
    // accepting prompts we can't service.
    let auth = auth::load()?;
    let session = session::open(&cwd, args.resume.as_deref())?;
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
        run_repl(Arc::clone(&agent)).await?;
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
async fn run_repl(agent: Arc<agent::Agent>) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(tokio::io::stdin());
    stdout
        .write_all(b"\x1b[1m\xf0\x9f\x90\x88\xef\xb8\x8f\xf0\x9f\x9a\x8c Catbus\x1b[0m \xe2\x80\x94 type a prompt, Ctrl-D to exit.\n\n")
        .await?;
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
