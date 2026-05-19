// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UNIX socket protocol. Each connection is one prompt → one
//! streamed response. Wire format is newline-delimited JSON. Each
//! request line is `{"kind":"prompt","text":"…"}` or
//! `{"kind":"set_plan_mode","on":true}`; each response line is one
//! of:
//!
//! * `{"kind":"started"}`                              — handshake
//! * `{"kind":"chunk","text":"…"}`                     — partial text
//! * `{"kind":"done","text":"…"}`                      — final answer
//! * `{"kind":"error","message":"…"}`                  — failure
//!
//! We don't actually stream from the Messages API today (the agent
//! returns the whole concatenated text), so `chunk` is reserved for
//! later. Clients should already handle it.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::agent::Agent;

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn serve(agent: Arc<Agent>, path: PathBuf) -> Result<(), SocketError> {
    // Stale socket from a crashed previous run blocks bind otherwise.
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("listening on {}", path.display());

    // Clean shutdown on SIGINT/SIGTERM so the socket file gets removed.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let agent = Arc::clone(&agent);
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, agent).await {
                                log::warn!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        log::warn!("accept failed: {e}");
                    }
                }
            }
            () = &mut shutdown => {
                log::info!("shutdown signal received");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn handle(stream: UnixStream, agent: Arc<Agent>) -> Result<(), SocketError> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    write_line(&mut write_half, &Response::Started).await?;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                write_line(
                    &mut write_half,
                    &Response::Error {
                        message: format!("malformed request: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };
        match req {
            Request::Prompt { text } => match agent.run_user_prompt(text).await {
                Ok(reply) => {
                    write_line(&mut write_half, &Response::Done { text: reply }).await?;
                }
                Err(e) => {
                    write_line(&mut write_half, &Response::Error { message: e.to_string() }).await?;
                }
            },
            Request::SetPlanMode { on } => {
                agent.set_plan_mode(on);
                write_line(
                    &mut write_half,
                    &Response::Done {
                        text: format!("plan-mode = {on}"),
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn write_line(stream: &mut tokio::net::unix::OwnedWriteHalf, resp: &Response) -> Result<(), SocketError> {
    let mut s = serde_json::to_string(resp).expect("Response is Serialize");
    s.push('\n');
    stream.write_all(s.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Request {
    Prompt { text: String },
    SetPlanMode { on: bool },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Response {
    Started,
    Done { text: String },
    Error { message: String },
}
