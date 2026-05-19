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
//! `/tabs/N/claude/messages` endpoint Just Works), and runs a small
//! Read / Write / Edit / Bash tool loop.

#![allow(clippy::module_name_repetitions)]

use std::path::PathBuf;

use clap::Parser;

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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
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
    let agent = agent::Agent::new(auth, session);
    socket::serve(agent, socket_path).await?;
    Ok(())
}
