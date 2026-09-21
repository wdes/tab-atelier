// SPDX-License-Identifier: MPL-2.0

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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::agent::Agent;
use crate::tools;

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Serve socket clients until shut down.
///
/// `once` makes the first connection the last one: the prompt is answered, the
/// socket file is removed, and the process returns so it can exit. That is how
/// the `Spawn` tool runs a sub-agent — see `--once` in `main`, which explains
/// why a self-terminating child is the only kind that cannot be orphaned.
pub async fn serve(agent: Arc<Agent>, path: PathBuf, once: bool) -> Result<(), SocketError> {
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
                        // With `--once` the connection is the whole purpose of
                        // the process, so it is awaited here rather than
                        // spawned: the loop then ends, the socket file is
                        // removed, and this agent exits on its own. A spawned
                        // child that instead waited to be reaped is what leaves a
                        // running agent behind when its parent dies mid-call.
                        if once {
                            if let Err(e) = handle(stream, agent).await {
                                log::warn!("connection error: {e}");
                            }
                            break;
                        }
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
            Request::Prompt { text } => {
                match run_watching_for_questions(&mut lines, &mut write_half, &agent, text).await {
                    Ok(Some(turn)) => {
                        write_line(
                            &mut write_half,
                            &Response::Done {
                                text: turn.answer,
                                // Carried beside the answer, never inside it: a client
                                // that does not know this field sees exactly the reply
                                // it saw before.
                                reasoning: turn.reasoning,
                            },
                        )
                        .await?;
                    }
                    // The client went away mid-turn: nothing to write to, so stop.
                    Ok(None) => break,
                    Err(e) => {
                        write_line(&mut write_half, &Response::Error { message: e.to_string() }).await?;
                    }
                }
            }
            // An answer with no turn running: the question is over, and saying so beats
            // silence so the client knows its answer was not used.
            Request::Answer { id, chosen, note } => {
                let _ = agent
                    .asker()
                    .answer(id, crate::tools::ask::Chosen { labels: chosen, note });
                write_line(
                    &mut write_half,
                    &Response::done("no question is open; the answer was not used"),
                )
                .await?;
            }
            Request::SetPlanMode { on } => {
                let gate = if on { tools::Gate::Plan } else { tools::Gate::Open };
                agent.set_gate(gate).await;
                write_line(&mut write_half, &Response::done(format!("gate = {}", gate.as_str()))).await?;
            }
            Request::SetGate { gate } => {
                let Some(parsed) = tools::parse_gate(&gate) else {
                    write_line(
                        &mut write_half,
                        &Response::Error {
                            message: format!("unknown gate {gate:?}; expected open, plan or auto"),
                        },
                    )
                    .await?;
                    continue;
                };
                agent.set_gate(parsed).await;
                write_line(&mut write_half, &Response::done(format!("gate = {}", parsed.as_str()))).await?;
            }
            Request::Clear => match agent.clear().await {
                // The previous id is in the reply so the client can offer a way
                // back — over a socket the user cannot see the agent's stderr,
                // so a bare "ok" would strand them.
                Ok(previous) => {
                    write_line(
                        &mut write_half,
                        &Response::done(format!(
                            "cleared; previous session ({}, {}) is still on disk",
                            previous.name,
                            previous.id.get(..8).unwrap_or(&previous.id)
                        )),
                    )
                    .await?;
                }
                Err(e) => {
                    write_line(&mut write_half, &Response::Error { message: e.to_string() }).await?;
                }
            },
        }
    }
    Ok(())
}

/// Run a prompt, sending any question that arises while it runs.
///
/// A question can only appear *during* a turn — `AskUserQuestion` is a tool, so it is called
/// from inside the loop — which means the connection is already busy running this prompt when
/// the question needs to go out. So the two are raced: the prompt future, and a poll for a
/// question to forward. Polling is the simple answer here and a poll is cheap; the alternative
/// is a channel from the asker to this function, which would be more plumbing than a 100ms
/// check of one `Mutex`.
///
/// Each question is sent once, tracked by its id: a question that stayed pending for minutes
/// must not be re-sent every tick, and the id is what changes when a new one replaces it.
async fn run_watching_for_questions(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    agent: &Arc<Agent>,
    text: String,
) -> Result<Option<crate::agent::Turn>, SocketError> {
    /// How often to look for a question. Fast enough to feel immediate, slow enough to be free.
    const POLL: std::time::Duration = std::time::Duration::from_millis(100);

    let mut run = std::pin::pin!(agent.run_user_prompt(text));
    let mut sent: Option<u64> = None;
    loop {
        tokio::select! {
            result = &mut run => {
                return Ok(Some(
                    result.map_err(|e| SocketError::Io(std::io::Error::other(e.to_string())))?,
                ));
            }
            // An answer arriving mid-turn. Reading it *here* is the part that is easy to get
            // wrong, and was: with only the prompt and the poll raced, the question went out,
            // the client answered, and the answer could never be read — because the request
            // loop that reads it is the very call blocked awaiting this prompt. The symptom is
            // a turn that hangs until the ask times out, with a question on the client's screen
            // and an answer already sent.
            line = lines.next_line() => {
                let Some(line) = line? else {
                    // The client hung up mid-turn. The turn is abandoned rather than left
                    // running with nobody to hear its answer.
                    return Ok(None);
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(Request::Answer { id, chosen, note }) => {
                        let said = if agent.asker().answer(
                            id,
                            crate::tools::ask::Chosen { labels: chosen, note },
                        ) {
                            "answered"
                        } else {
                            "that question is no longer open; the answer was not used"
                        };
                        write_line(write_half, &Response::done(said)).await?;
                    }
                    Ok(other) => {
                        write_line(
                            write_half,
                            &Response::Error {
                                message: format!(
                                    "a turn is already running, so `{}` cannot be handled yet — \
                                     the only request that works mid-turn is `answer`.",
                                    request_name(&other)
                                ),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        write_line(
                            write_half,
                            &Response::Error {
                                message: format!("malformed request: {e}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            () = tokio::time::sleep(POLL) => {
                if let Some((id, questions)) = agent.asker().pending()
                    && sent != Some(id)
                {
                    // A write failure here must not lose the turn: the question is dropped and
                    // the ask times out, which it already handles.
                    if write_line(write_half, &Response::Question { id, questions }).await.is_err() {
                        log::warn!("could not send a question; the client will not see it");
                    }
                    sent = Some(id);
                }
            }
        }
    }
}

/// A request's name, for a message that says which one was refused.
const fn request_name(req: &Request) -> &'static str {
    match req {
        Request::Prompt { .. } => "prompt",
        Request::Answer { .. } => "answer",
        Request::SetPlanMode { .. } => "set_plan_mode",
        Request::SetGate { .. } => "set_gate",
        Request::Clear => "clear",
    }
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
    Prompt {
        text: String,
    },
    /// The original two-state form, kept working.
    ///
    /// A client that only knows about plan-mode still decides
    /// `Plan` vs `Open` and cannot express auto — which is the correct
    /// degradation: it never asked for the third state, so it must not get it.
    SetPlanMode {
        on: bool,
    },
    /// The three-state form. `"open"`, `"plan"` or `"auto"`; anything else is
    /// refused rather than defaulted, so a typo cannot silently mean "open".
    SetGate {
        gate: String,
    },
    /// Start a fresh session, leaving the current transcript on disk.
    ///
    /// The counterpart of the REPL's `/clear`, so a client that cannot type a
    /// slash command — the tab-atelier GUI, a phone — can offer the same thing.
    /// Nothing is deleted, which is what makes it safe to expose over a socket:
    /// the reply names the transcript that was left behind so the client can
    /// show the user how to get it back.
    Clear,
    /// Answer the question a [`Response::Question`] asked.
    ///
    /// The `id` is echoed from that response, so an answer arriving after its question has
    /// expired — or after a second one replaced it — is ignored rather than delivered to the
    /// wrong question. `chosen` is one list of labels per question, in the order asked.
    Answer {
        id: u64,
        chosen: Vec<Vec<String>>,
        /// Free text the operator attached to the whole reply — the one thing a fixed list of
        /// labels cannot express. Optional, so a client that predates it is unaffected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Response {
    Started,
    Done {
        text: String,
        /// The model's own deliberation, when it produced any.
        ///
        /// A sibling field rather than part of `text`, so a client that predates
        /// it — the tab-atelier API, the phone — reads exactly the reply it read
        /// before. Empty for every reply that is not a turn's answer, and skipped
        /// on the wire when empty, so nothing changes for a model that does not
        /// think. See `agent::Turn`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
    },
    Error {
        message: String,
    },
    /// A question from [`crate::tools::ask`], waiting for an [`Request::Answer`].
    ///
    /// Carries the `id` the answer must echo, so an answer that arrives after this question
    /// has expired is ignored rather than delivered to the next one.
    Question {
        id: u64,
        questions: Vec<crate::tools::ask::Question>,
    },
}

impl Response {
    /// A reply with no reasoning attached — every response that is not a turn's
    /// answer: a gate change, a clear, a handshake.
    fn done(text: impl Into<String>) -> Self {
        Self::Done {
            text: text.into(),
            reasoning: String::new(),
        }
    }
}

/// Whether something is already listening on `path`.
///
/// Connecting is the only honest test. A socket *file* survives a crash, so its
/// presence says nothing about whether an agent is behind it, and treating a
/// leftover file as "an agent is running" would refuse to start an agent at all
/// after any unclean exit. A connect to a file with no listener fails with
/// `ECONNREFUSED`; a connect to a live one succeeds.
///
/// Used at start-up to keep two agents off one session — see the guard in `main`.
/// The connection is closed immediately and nothing is sent, so a peer sees a
/// client that connected and went away. That is harmless for a normal agent (it
/// logs a warning), but a peer run with `--once` treats that connection as its
/// one and exits, so this is only safe to call against a socket this process is
/// deciding whether to take over — never as a general liveness poll.
#[must_use]
pub fn is_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_file_with_no_listener_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");

        // Absent: nothing there at all.
        assert!(!is_live(&path));

        // A leftover file with no listener — what an agent killed uncleanly
        // leaves behind. This is the case the start-up guard depends on getting
        // right: reading it as live would refuse to start an agent after every
        // crash, which is worse than the collision it guards against.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the socket file outlives its listener");
        assert!(!is_live(&path), "a stale socket file must not read as live");
    }

    #[test]
    fn a_socket_with_a_listener_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_live(&path));
        drop(listener);
        assert!(!is_live(&path), "and stops reading as live when the listener goes");
    }
}
