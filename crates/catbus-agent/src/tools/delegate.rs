// SPDX-License-Identifier: MPL-2.0

//! Send a sub-prompt to another catbus-agent and wait for its reply.
//!
//! Sync RPC over the target's UNIX socket: connect, send one
//! `{"kind":"prompt"…}` line, read NDJSON responses until we see a
//! `done` or `error`. Skip the `started` handshake the server emits
//! immediately on accept.
//!
//! Intentionally simple: no chaining of streaming chunks, no shared
//! context — the target gets only the prompt the caller hands over.
//! Long delegated calls eat into the model's tool-loop budget; the
//! caller can pass `timeout_secs` (defaults to 5 min, hard-capped at
//! 30 min).
//!
//! [`rpc`] and [`wait_ready`] are public to [`super::spawn`], which starts a
//! child agent and then has to talk to it over exactly this protocol. They live
//! here rather than being written a second time because the framing rules — skip
//! `started`, stop at `done`/`error`, never invent a second reply — are the part
//! that has to agree between the two callers, and a drifted copy would look like
//! a flaky agent rather than a protocol bug.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);
pub const MAX_TIMEOUT: Duration = Duration::from_mins(30);
/// How often [`rpc_starting`] retries a socket that is not accepting yet.
const READY_POLL: Duration = Duration::from_millis(50);

pub async fn run(input: &serde_json::Value, _cwd: &Path) -> Result<String, String> {
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing prompt".to_string())?;
    let socket_path = resolve_target(input)?;
    rpc(&socket_path, prompt, requested_timeout(input)).await
}

/// The timeout the caller asked for, clamped to the cap.
///
/// Shared with [`super::spawn`] so an operator gets the same ceiling and the
/// same `timeout_secs` spelling whether they are prompting a peer or starting
/// one.
#[must_use]
pub fn requested_timeout(input: &serde_json::Value) -> Duration {
    input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map(Duration::from_secs)
        .map_or(DEFAULT_TIMEOUT, |d| d.min(MAX_TIMEOUT))
}

/// An open connection whose `started` frame has been consumed: the write half,
/// and the line reader positioned at the first reply frame.
type Wire = (OwnedWriteHalf, Lines<BufReader<OwnedReadHalf>>);

/// Connect and consume the `started` frame the server sends on accept.
async fn connect_handshaken(socket_path: &Path) -> Result<Wire, String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect {}: {e}", socket_path.display()))?;
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // The server greets us the moment we connect. Consume it before sending so
    // request/reply pairs stay aligned.
    loop {
        let Some(line) = lines.next_line().await.map_err(|e| format!("read: {e}"))? else {
            return Err("connection closed before handshake".to_string());
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(frame) = serde_json::from_str::<Frame>(&line) {
            match frame {
                Frame::Started => break,
                Frame::Error { message } => return Err(format!("agent error: {message}")),
                // Unexpected ordering — keep reading until we get a Started or
                // an Error.
                Frame::Chunk { .. } | Frame::Done { .. } => {}
            }
        }
    }
    Ok((write_half, lines))
}

/// Send one prompt over a handshaken connection and read until `done`.
async fn converse(wire: Wire, prompt: &str) -> Result<String, String> {
    let (mut write_half, mut lines) = wire;

    let req = serde_json::json!({ "kind": "prompt", "text": prompt });
    let mut payload = req.to_string();
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    write_half.flush().await.map_err(|e| format!("flush: {e}"))?;

    loop {
        let Some(line) = lines.next_line().await.map_err(|e| format!("read: {e}"))? else {
            return Err("connection closed before reply".to_string());
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Frame>(&line).map_err(|e| format!("malformed frame `{line}`: {e}"))? {
            Frame::Done { text } => return Ok(text),
            Frame::Error { message } => return Err(format!("agent error: {message}")),
            Frame::Started | Frame::Chunk { .. } => {}
        }
    }
}

/// Send one prompt to the agent listening on `socket_path` and wait for its
/// reply.
///
/// No retry: the target is an agent that is already running, so a connect that
/// fails means it is not there, and failing at once is the useful answer. A
/// target that is still starting up wants [`rpc_starting`]. `timeout` covers the
/// whole exchange, connect included.
pub async fn rpc(socket_path: &Path, prompt: &str, timeout: Duration) -> Result<String, String> {
    let work = async {
        let wire = connect_handshaken(socket_path).await?;
        converse(wire, prompt).await
    };

    bounded(work, timeout, socket_path).await
}

/// Send one prompt to an agent that is *starting up*, using a single connection
/// for both readiness and the prompt.
///
/// A freshly started child creates its socket file before it is listening, so a
/// connect can land on a socket with nothing behind it yet and retrying is the
/// only way to tell "not up yet" from "never started". The retry has to reuse the
/// connection it succeeds on, because a sub-agent runs with `--once`: the first
/// connection it accepts is the one it answers and then exits on. Connecting once
/// to check readiness and again to send the prompt spends the single connection
/// on the check and then finds the child gone — which read as
/// `Connection reset by peer`, and was the bug this signature exists to prevent.
///
/// `ready` bounds the wait for the socket; `timeout` bounds the exchange after it.
pub async fn rpc_starting(
    socket_path: &Path,
    prompt: &str,
    ready: Duration,
    timeout: Duration,
) -> Result<String, String> {
    let connecting = std::time::Instant::now();

    loop {
        match connect_handshaken(socket_path).await {
            Ok(wire) => return bounded(converse(wire, prompt), timeout, socket_path).await,
            // Checked here rather than at the top of the loop, so the deadline
            // message always quotes the failure that just happened instead of
            // needing a remembered one.
            Err(why) => {
                if connecting.elapsed() >= ready {
                    return Err(format!(
                        "agent at {} was not ready within {}s: {why}",
                        socket_path.display(),
                        ready.as_secs()
                    ));
                }
            }
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Apply the exchange timeout, with a message that names the socket.
async fn bounded<F>(work: F, timeout: Duration, socket_path: &Path) -> Result<String, String>
where
    F: std::future::Future<Output = Result<String, String>>,
{
    tokio::time::timeout(timeout, work).await.unwrap_or_else(|_| {
        Err(format!(
            "delegated call to {} timed out after {}s",
            socket_path.display(),
            timeout.as_secs()
        ))
    })
}

/// Resolve `target` (session-id, socket-path, or relative path) into
/// a concrete socket file. Session-id lookups scan
/// `~/.claude/projects/*/<id>.sock`.
fn resolve_target(input: &serde_json::Value) -> Result<PathBuf, String> {
    let target = input
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing target (session id or socket path)".to_string())?;

    let direct = PathBuf::from(target);
    if direct.is_absolute() && direct.exists() {
        return Ok(direct);
    }
    if !target.contains('/') {
        let home = std::env::var_os("HOME").ok_or_else(|| "no $HOME".to_string())?;
        let projects = PathBuf::from(home).join(".claude").join("projects");
        let Ok(read_dir) = std::fs::read_dir(&projects) else {
            return Err(format!("no agents found (couldn't read {})", projects.display()));
        };
        for entry in read_dir.flatten() {
            let candidate = entry.path().join(format!("{target}.sock"));
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "couldn't resolve target `{target}` to a socket — pass a session id or absolute socket path"
    ))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Frame {
    Started,
    Chunk {
        #[allow(dead_code)] // future: streaming relay through to caller.
        text: String,
    },
    Done {
        text: String,
    },
    Error {
        message: String,
    },
}
