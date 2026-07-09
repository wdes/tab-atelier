// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Discovery + parsing of catbus-agent session transcripts (with
//! the legacy Claude Code TUI as a recognised fallback).
//!
//! Sessions persist at `~/.claude/projects/{escaped-cwd}/{session-id}
//! .jsonl`, where the escaping rule is "every non-ASCII-alphanumeric
//! byte → `-`". The file is append-only, one JSON object per line,
//! mixing meta entries (permission mode, file-history snapshots)
//! with the conversation itself (`type = "user" | "assistant"`).
//!
//! The mobile remote treats each agent-running tab as a chat thread:
//! this module is what turns a tab's shell PID into the transcript
//! file and walks the file into a flat list of messages the remote
//! can render as chat bubbles. It also speaks the catbus-agent
//! NDJSON socket protocol so `POST /tabs/N/catbus/message` can
//! forward prompts.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One detected agent session attached to a tab.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSession {
    pub session_id: String,
    pub file_path: PathBuf,
    pub cwd: PathBuf,
    /// Agent process PID — handy for kill / signal later.
    pub agent_pid: u32,
}

/// A single conversation turn after the transcript has been flattened.
#[derive(Debug, Clone, Serialize)]
pub struct ParsedMessage {
    pub role: String,
    /// Each segment is one rendered "block" in the chat: a plain text
    /// paragraph, a tool invocation header, or a tool result body.
    pub segments: Vec<MessageSegment>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSegment {
    Text { text: String },
    ToolUse { name: String, input: String },
    ToolResult { text: String, is_error: bool },
    Thinking { text: String },
}

/// Walk descendant processes of `shell_pid` looking for an agent
/// runtime — catbus-agent (preferred) or the legacy Claude Code TUI
/// as a fallback. Both write the same JSONL layout under
/// `~/.claude/projects/`, so a single lookup serves the /catbus
/// endpoints regardless of which one the tab is hosting.
pub fn find_session(shell_pid: u32) -> Option<AgentSession> {
    find_session_for(find_agent_descendant(shell_pid)?)
}

/// [`find_session`] when the agent's pid is already known (e.g. from the
/// same tick's [`agent_activity_with_pid`] walk) — skips the BFS over the
/// shell's whole /proc subtree that `find_session` would repeat. A stale
/// pid (agent restarted since the walk) fails the `/proc` reads and
/// returns `None`; the caller's next sweep refreshes it.
pub fn find_session_for(agent_pid: u32) -> Option<AgentSession> {
    let cwd = fs::read_link(format!("/proc/{agent_pid}/cwd")).ok()?;
    let project_dir = home_projects_dir()?.join(escape_cwd(&cwd));
    if !project_dir.is_dir() {
        return None;
    }
    let (path, session_id) = newest_session(&project_dir)?;
    Some(AgentSession {
        session_id,
        file_path: path,
        cwd,
        agent_pid,
    })
}

/// Returns true when the agent CLI under `shell_pid` has at least
/// one child process — i.e. it's actively running a tool subprocess
/// (Bash, Read, …). Used by the LED sweep to keep the indicator on
/// "thinking" for as long as a tool is alive, even when the
/// `PostToolUse` / `PreToolUse` hook cadence leaves a quiet window
/// longer than the 2-min staleness sweep (long `cargo build`, sleep,
/// pytest run, …).
#[must_use]
/// Liveness + work state of a tab's agent session, from a single `/proc` walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentActivity {
    /// No `catbus-agent` / `claude` process under the shell — the session ended.
    Gone,
    /// Agent alive but no subprocess is on-CPU: idle, waiting for input, or
    /// thinking over the network — not running local work.
    Idle,
    /// A descendant is actually running (`R`) or blocked in uninterruptible I/O
    /// (`D`) — a real tool call (cargo build, pytest, …) is underway.
    Working,
}

/// Classify a tab's agent session in one BFS over the agent's descendants.
///
/// `Working` requires a descendant genuinely on-CPU (`R`) or in uninterruptible
/// I/O (`D`); merely-existing children don't count. An idle agent keeps
/// persistent helpers alive (MCP servers, a language server, a paused shell)
/// that sit in `S` (interruptible sleep), and its own status hooks flit through
/// `R` — treating either as work is what pinned idle tabs to the green
/// "thinking" LED with nothing running.
/// Also returns the agent CLI's pid when one was found, so a caller in
/// the same tick can resolve the session ([`find_session_for`]) without
/// re-walking the identical subtree.
pub fn agent_activity_with_pid(shell_pid: u32) -> (AgentActivity, Option<u32>) {
    use std::fmt::Write as _;
    let Some(agent_pid) = find_agent_descendant(shell_pid) else {
        return (AgentActivity::Gone, None);
    };
    let mut path = String::with_capacity(48);
    let mut queue = vec![agent_pid];
    while let Some(pid) = queue.pop() {
        // Skip the agent process itself; we're judging its subprocesses.
        if pid != agent_pid && matches!(proc_state(pid), Some('R' | 'D')) {
            return (AgentActivity::Working, Some(agent_pid));
        }
        path.clear();
        let _ = write!(path, "/proc/{pid}/task/{pid}/children");
        if let Ok(raw) = fs::read_to_string(&path) {
            queue.extend(raw.split_ascii_whitespace().filter_map(|s| s.parse::<u32>().ok()));
        }
    }
    (AgentActivity::Idle, Some(agent_pid))
}

/// The scheduler state character (`R`/`S`/`D`/`Z`/`T`/…) from `/proc/<pid>/stat`,
/// or `None` if the process is gone.
fn proc_state(pid: u32) -> Option<char> {
    parse_proc_state(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Pull the state char out of a `/proc/<pid>/stat` line. The format is
/// `pid (comm) STATE …`, and `comm` may itself contain spaces and parens, so we
/// key off the *last* `)` rather than splitting on whitespace.
fn parse_proc_state(stat: &str) -> Option<char> {
    let after_comm = stat.rfind(')')? + 2; // skip ") "
    stat.get(after_comm..)?.chars().next()
}

/// Owner UID of a `/proc/<pid>` entry (the process's real UID), or
/// `None` if the entry is gone. Safe-Rust via `MetadataExt::uid` — no
/// `geteuid` FFI needed (the crate denies `unsafe`).
fn proc_uid(pid: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
}

/// BFS over `/proc/{pid}/task/{pid}/children`. Match `catbus-agent`
/// or the legacy `claude` TUI by `comm`. We don't recurse into
/// kernel threads or pids in different namespaces — sticking to
/// /proc handles this for us, those entries simply don't exist.
fn find_agent_descendant(root_pid: u32) -> Option<u32> {
    use std::fmt::Write as _;
    const AGENT_COMMS: &[&str] = &["catbus-agent", "claude"];
    // Only trust processes owned by the same UID as this daemon. A
    // descendant can freely rename its own `comm` to "claude" /
    // "catbus-agent"; without this check, on a shared host another
    // user's process matching by name could steer transcript/socket
    // resolution to a path of its choosing. `/proc/self` ownership is
    // the daemon's real UID; compare each candidate against it.
    let self_uid = proc_uid("self");
    // Reused across both /proc reads per BFS step so neither
    // `read_comm` nor `read_children` allocates a fresh path String.
    let mut path = String::with_capacity(48);
    let mut queue = vec![root_pid];
    while let Some(pid) = queue.pop() {
        path.clear();
        let _ = write!(path, "/proc/{pid}/comm");
        if let Ok(mut comm) = fs::read_to_string(&path) {
            if comm.ends_with('\n') {
                comm.pop();
            }
            if AGENT_COMMS.contains(&comm.as_str()) && proc_uid(&pid.to_string()) == self_uid && self_uid.is_some() {
                return Some(pid);
            }
        }
        path.clear();
        let _ = write!(path, "/proc/{pid}/task/{pid}/children");
        if let Ok(raw) = fs::read_to_string(&path) {
            queue.extend(raw.split_ascii_whitespace().filter_map(|s| s.parse::<u32>().ok()));
        }
    }
    None
}

fn home_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// Escaping rule used by both catbus-agent and Claude Code when
/// mapping a cwd to a project directory name: every byte that isn't
/// `[A-Za-z0-9]` becomes `-`. Consecutive non-alphanumerics produce
/// consecutive dashes (e.g. `/@` → `--`).
#[must_use]
pub fn escape_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Pick the JSONL file with the latest modified-time in the project
/// directory. Multiple sessions in the same cwd are unusual but
/// possible (multiple agent tabs in the same dir); the newest one
/// is the most likely current session.
fn newest_session(project_dir: &Path) -> Option<(PathBuf, String)> {
    let mut best: Option<(PathBuf, String, std::time::SystemTime)> = None;
    for entry in fs::read_dir(project_dir).ok()?.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry.metadata().and_then(|m| m.modified()).ok()?;
        if best.as_ref().is_none_or(|(_, _, t)| mtime > *t) {
            best = Some((path.clone(), stem.to_string(), mtime));
        }
    }
    best.map(|(p, id, _)| (p, id))
}

/// Convenience wrapper that returns every parsed message — equivalent
/// to `parse_messages_since(path, 0)`. Used by tests and any caller
/// that wants the full transcript.
#[cfg(test)]
pub fn parse_messages(file_path: &Path) -> Vec<ParsedMessage> {
    parse_messages_since(file_path, 0)
}

/// Read every conversation entry from `file_path`, skipping the first
/// `since` parsed messages without keeping them in memory. Used by
/// `/catbus/messages?since=N` so the mobile remote's incremental polls
/// don't force a full-file re-allocation on every tick.
pub fn parse_messages_since(file_path: &Path, since: usize) -> Vec<ParsedMessage> {
    use std::io::BufRead;
    let Ok(file) = fs::File::open(file_path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::with_capacity(64);
    let mut parsed = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(&line) else {
            continue;
        };
        let role = match raw.r#type.as_str() {
            "user" | "assistant" => raw.r#type.clone(),
            _ => continue,
        };
        let Some(msg) = raw.message else { continue };
        let segments = match msg.content {
            // Plain user prompts come through as a bare string.
            RawContent::String(s) => vec![MessageSegment::Text { text: s }],
            RawContent::Blocks(blocks) => blocks.into_iter().filter_map(parse_block).collect(),
        };
        if segments.is_empty() {
            continue;
        }
        if parsed >= since {
            out.push(ParsedMessage {
                role,
                segments,
                timestamp: raw.timestamp,
            });
        }
        parsed += 1;
    }
    out
}

fn parse_block(block: RawBlock) -> Option<MessageSegment> {
    match block.r#type.as_str() {
        "text" => block.text.map(|t| MessageSegment::Text { text: t }),
        "thinking" => block.thinking.map(|t| MessageSegment::Thinking { text: t }),
        "tool_use" => {
            let input = block
                .input
                .map(|v| serde_json::to_string(&v).unwrap_or_default())
                .unwrap_or_default();
            block.name.map(|name| MessageSegment::ToolUse { name, input })
        }
        "tool_result" => {
            let is_error = block.is_error.unwrap_or(false);
            // Tool result `content` can be a plain string or an array
            // of `{type:"text",text}` blocks; flatten both into one
            // chunk of text for the chat view.
            let text = match block.content {
                Some(RawContent::String(s)) => s,
                Some(RawContent::Blocks(inner)) => inner
                    .into_iter()
                    .filter_map(|b| b.text.or(b.thinking))
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            };
            Some(MessageSegment::ToolResult { text, is_error })
        }
        _ => None,
    }
}

// --- Raw transcript shapes -------------------------------------------------

#[derive(Deserialize)]
struct RawLine {
    r#type: String,
    message: Option<RawMessage>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    content: RawContent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    String(String),
    Blocks(Vec<RawBlock>),
}

#[derive(Deserialize)]
struct RawBlock {
    r#type: String,
    text: Option<String>,
    thinking: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
    content: Option<RawContent>,
    is_error: Option<bool>,
}

/// Read the `{session-id}.tokens.json` sidecar written by catbus-agent
/// after each prompt. Returns `None` when no agent session is running
/// or the file doesn't exist yet.
#[must_use]
pub fn read_session_tokens(session: &AgentSession) -> Option<crate::TokenUsage> {
    let path = session.file_path.with_extension("tokens.json");
    let data = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    Some(crate::TokenUsage {
        input: v.get("input").and_then(serde_json::Value::as_u64).unwrap_or(0),
        output: v.get("output").and_then(serde_json::Value::as_u64).unwrap_or(0),
    })
}

/// Open the per-session UNIX socket, send a `prompt` frame, and
/// block until the agent emits a `done` or `error` reply. Used by
/// `POST /tabs/N/catbus/message` to forward the mobile remote's
/// prompts. Sync (synchronous `std::os::unix::net::UnixStream`)
/// because the rest of the api crate is on threads, not tokio.
///
/// `socket_path` is typically `{transcript-stem}.sock` — i.e. the
/// session's transcript with the extension swapped to `.sock`.
pub fn send_prompt_to_socket(socket_path: &Path, text: &str) -> Result<String, String> {
    use serde_json::Value;
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("connect {}: {e}", socket_path.display()))?;
    // 10-minute ceiling — agent answers shouldn't take longer; if
    // they do, something's wrong on the agent side and we'd rather
    // free the API thread than hang it forever.
    let timeout = Duration::from_mins(10);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut writer = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::new(stream);

    // The server greets every connection with {"kind":"started"} —
    // drain it before sending, so frames stay request/reply aligned.
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("read greeting: {e}"))?;

    let payload = format!(r#"{{"kind":"prompt","text":{}}}"#, json_encode_string(text));
    writeln!(writer, "{payload}").map_err(|e| format!("write: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;

    // Read until we see a `done`/`error`. Other frame kinds
    // (`chunk`, future additions) are ignored — we only need the
    // final answer for now.
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("agent closed connection before replying".into());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame: Value = serde_json::from_str(trimmed).map_err(|e| format!("malformed frame: {e}"))?;
        match frame.get("kind").and_then(Value::as_str) {
            Some("done") => {
                return Ok(frame
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string());
            }
            Some("error") => {
                return Err(frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string());
            }
            _ => {}
        }
    }
}

/// JSON-encode a string for inline injection. We don't pull in
/// `serde_json::to_string` for the whole payload because that'd
/// build a full `Value`; the text is the only field that needs
/// escaping.
fn json_encode_string(s: &str) -> String {
    // Serialising a plain string never fails; the fallback (a valid empty JSON
    // string) is unreachable but keeps this panic-free.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cwd_replaces_every_non_alpha_with_dash() {
        let p = Path::new("/mnt/Dev/@wdes");
        assert_eq!(escape_cwd(p), "-mnt-Dev--wdes");
    }

    #[test]
    fn parse_proc_state_keys_off_the_last_paren() {
        // Normal case.
        assert_eq!(parse_proc_state("1234 (bash) S 1 1234 …"), Some('S'));
        assert_eq!(parse_proc_state("42 (cargo) R 1 …"), Some('R'));
        // comm containing spaces AND parens — must use the LAST ')'.
        assert_eq!(parse_proc_state("77 (weird )( name) D 1 …"), Some('D'));
        assert_eq!(parse_proc_state("88 (a) b) c) Z 1 …"), Some('Z'));
        // Malformed / empty.
        assert_eq!(parse_proc_state("no parens here"), None);
        assert_eq!(parse_proc_state("5 (x)"), None);
    }

    #[test]
    fn escape_cwd_handles_unicode() {
        // `Téléchargements` → each `é` becomes `-`
        let p = Path::new("/home/u/T\u{00e9}l\u{00e9}chargements");
        assert_eq!(escape_cwd(p), "-home-u-T-l-chargements");
    }

    #[test]
    fn parse_messages_skips_meta_lines() {
        let dir = std::env::temp_dir().join(format!("ta-catbus-test-{}-skip", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(
            &file,
            concat!(
                r#"{"type":"permission-mode","permissionMode":"default"}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"hello"},"timestamp":"t1"}"#,
                "\n",
                r#"{"type":"file-history-snapshot","messageId":"x"}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"t2"}"#,
                "\n",
            ),
        )
        .unwrap();
        let msgs = parse_messages(&file);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert!(matches!(msgs[0].segments[0], MessageSegment::Text { ref text } if text == "hello"));
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn parse_messages_extracts_tool_use_and_result() {
        let dir = std::env::temp_dir().join(format!("ta-catbus-test-{}-tools", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(
            &file,
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"foo\nbar","is_error":false}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let msgs = parse_messages(&file);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(
            &msgs[0].segments[0],
            MessageSegment::ToolUse { name, .. } if name == "Bash"
        ));
        assert!(matches!(
            &msgs[1].segments[0],
            MessageSegment::ToolResult { text, .. } if text == "foo\nbar"
        ));
    }
}
