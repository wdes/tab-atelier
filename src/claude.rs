// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Discovery + parsing of Claude Code session transcripts.
//!
//! Claude Code persists every session at
//! `~/.claude/projects/{escaped-cwd}/{session-id}.jsonl`, where the
//! escaping rule is "every non-ASCII-alphanumeric byte → `-`". The
//! file is append-only, one JSON object per line, mixing meta entries
//! (permission mode, file-history snapshots) with the conversation
//! itself (`type = "user" | "assistant"`).
//!
//! The mobile remote treats each Claude-running tab as a chat thread:
//! this module is what turns a tab's shell PID into the transcript
//! file and walks the file into a flat list of messages the remote
//! can render as chat bubbles.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One detected Claude Code session attached to a tab.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSession {
    pub session_id: String,
    pub file_path: PathBuf,
    pub cwd: PathBuf,
    /// `claude` process PID — handy for kill / signal later.
    pub claude_pid: u32,
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

/// Walk descendant processes of `shell_pid` looking for any agent
/// runtime — Claude Code's `claude` TUI *or* our own `catbus-agent`.
/// Both write the same JSONL layout under `~/.claude/projects/`, so a
/// single lookup serves the `/catbus` endpoints regardless of which
/// implementation the tab is hosting.
pub fn find_session(shell_pid: u32) -> Option<ClaudeSession> {
    let agent_pid = find_agent_descendant(shell_pid)?;
    let cwd = fs::read_link(format!("/proc/{agent_pid}/cwd")).ok()?;
    let project_dir = home_projects_dir()?.join(escape_cwd(&cwd));
    if !project_dir.is_dir() {
        return None;
    }
    let (path, session_id) = newest_session(&project_dir)?;
    Some(ClaudeSession {
        session_id,
        file_path: path,
        cwd,
        claude_pid: agent_pid,
    })
}

/// BFS over `/proc/{pid}/task/{pid}/children`. Match `claude` (the
/// Claude Code TUI) or `catbus-agent` (our own runtime). We don't
/// recurse into kernel threads or pids in different namespaces —
/// sticking to /proc handles this for us, those entries simply don't
/// exist.
fn find_agent_descendant(root_pid: u32) -> Option<u32> {
    const AGENT_COMMS: &[&str] = &["claude", "catbus-agent"];
    let mut queue = vec![root_pid];
    while let Some(pid) = queue.pop() {
        if let Some(comm) = read_comm(pid)
            && AGENT_COMMS.contains(&comm.as_str())
        {
            return Some(pid);
        }
        if let Some(kids) = read_children(pid) {
            queue.extend(kids);
        }
    }
    None
}

fn read_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end_matches('\n').to_string())
}

fn read_children(pid: u32) -> Option<Vec<u32>> {
    let raw = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    Some(raw.split_ascii_whitespace().filter_map(|s| s.parse().ok()).collect())
}

fn home_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// Claude Code's escaping rule: every byte that isn't `[A-Za-z0-9]`
/// becomes `-`. Consecutive non-alphanumerics produce consecutive
/// dashes (e.g. `/@` → `--`).
#[must_use]
pub fn escape_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Pick the JSONL file with the latest modified-time in the project
/// directory. Multiple sessions in the same cwd are unusual but
/// possible (multiple Claude tabs in the same dir); the newest one
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

/// Read every line of `file_path` and turn each conversation entry
/// into a `ParsedMessage`. Skips meta entries (permission-mode,
/// snapshots, anything that isn't an obvious user/assistant turn).
pub fn parse_messages(file_path: &Path) -> Vec<ParsedMessage> {
    let Ok(text) = fs::read_to_string(file_path) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(64);
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(line) else {
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
        out.push(ParsedMessage {
            role,
            segments,
            timestamp: raw.timestamp,
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cwd_replaces_every_non_alpha_with_dash() {
        let p = Path::new("/mnt/Dev/@wdes");
        assert_eq!(escape_cwd(p), "-mnt-Dev--wdes");
    }

    #[test]
    fn escape_cwd_handles_unicode() {
        // `Téléchargements` → each `é` becomes `-`
        let p = Path::new("/home/u/T\u{00e9}l\u{00e9}chargements");
        assert_eq!(escape_cwd(p), "-home-u-T-l-chargements");
    }

    #[test]
    fn parse_messages_skips_meta_lines() {
        let dir = std::env::temp_dir().join(format!("ta-claude-test-{}-skip", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("ta-claude-test-{}-tools", std::process::id()));
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
