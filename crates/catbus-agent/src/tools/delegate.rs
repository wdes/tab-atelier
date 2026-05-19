// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_TIMEOUT: Duration = Duration::from_mins(30);

pub async fn run(input: &serde_json::Value, _cwd: &Path) -> Result<String, String> {
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing prompt".to_string())?;
    let socket_path = resolve_target(input)?;
    let timeout = input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map(Duration::from_secs)
        .map_or(DEFAULT_TIMEOUT, |d| d.min(MAX_TIMEOUT));

    let work = async {
        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| format!("connect {}: {e}", socket_path.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        // The server greets us with {"kind":"started"} the moment we
        // connect. Consume it before sending so request/reply pairs
        // stay aligned.
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
                    // Unexpected ordering — keep reading until we
                    // get a Started or Error.
                    Frame::Chunk { .. } | Frame::Done { .. } => {}
                }
            }
        }

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
            match serde_json::from_str::<Frame>(&line)
                .map_err(|e| format!("malformed frame `{line}`: {e}"))?
            {
                Frame::Done { text } => return Ok(text),
                Frame::Error { message } => return Err(format!("agent error: {message}")),
                Frame::Started | Frame::Chunk { .. } => {}
            }
        }
    };

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
    Done { text: String },
    Error { message: String },
}
