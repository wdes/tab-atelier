// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session lifecycle + JSONL transcript writer.
//!
//! Catbus writes the same JSONL files Claude Code does, in the same
//! place (`~/.claude/projects/{escaped-cwd}/{session-id}.jsonl`).
//! That keeps the existing tab-atelier `/tabs/N/catbus/messages`
//! endpoint working unchanged — it doesn't know or care whether the
//! transcript came from `claude` or from us.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no $HOME — can't locate ~/.claude/projects")]
    NoHome,
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub project_dir: PathBuf,
    /// Human-readable label stored in `{id}.name` alongside the transcript.
    /// Empty string means unnamed.
    pub name: Mutex<String>,
    file: Mutex<File>,
    last_uuid: Mutex<Option<String>>,
}

/// Open (or resume) a session.
///
/// Decision tree:
///   * Explicit `Some(id)` → resume that exact session.
///   * `None` + `new_session = true` → fresh UUID, fresh transcript.
///   * `None` + `new_session = false` (default) → resume the newest
///     `.jsonl` in the project directory if one exists; otherwise
///     start fresh. This is the "I closed the tab, I reopened the
///     tab, please pick up where I left off" path.
///
/// `last_uuid` is seeded from the resumed transcript's last entry
/// so `parentUuid` chaining stays intact across the restart.
pub fn open(cwd: &Path, resume_id: Option<&str>, new_session: bool) -> Result<Session, SessionError> {
    let home = std::env::var_os("HOME").ok_or(SessionError::NoHome)?;
    let project_dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(escape_cwd(cwd));
    std::fs::create_dir_all(&project_dir)?;
    // Three-way decision; `map_or_else` would obscure it.
    #[allow(clippy::option_if_let_else)]
    let id = if let Some(id) = resume_id {
        id.to_string()
    } else if new_session {
        Uuid::new_v4().to_string()
    } else {
        latest_session_id(&project_dir).unwrap_or_else(|| Uuid::new_v4().to_string())
    };
    let transcript = project_dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(&transcript)?;
    let last_uuid = last_entry_uuid(&transcript).ok().flatten();
    let name = load_session_name(&project_dir, &id);
    Ok(Session {
        id,
        cwd: cwd.to_path_buf(),
        project_dir,
        name: Mutex::new(name),
        file: Mutex::new(file),
        last_uuid: Mutex::new(last_uuid),
    })
}

/// Load the `.name` sidecar for a session, if it exists and is non-empty.
fn load_session_name(project_dir: &Path, id: &str) -> String {
    let path = project_dir.join(format!("{id}.name"));
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Newest `.jsonl` stem in `dir`, ignoring zero-byte files (those
/// are sessions that were opened but never written to — typically
/// crashes immediately after start). Returns the session id.
fn latest_session_id(dir: &Path) -> Option<String> {
    let mut best: Option<(String, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() == 0 {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
            best = Some((stem.to_string(), mtime));
        }
    }
    best.map(|(id, _)| id)
}

/// Read the last non-empty JSONL line of `path` and pluck its
/// `uuid`. Used to chain `parentUuid` correctly on resume.
fn last_entry_uuid(path: &Path) -> std::io::Result<Option<String>> {
    let text = std::fs::read_to_string(path)?;
    for line in text.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(uuid) = v.get("uuid").and_then(|u| u.as_str()) {
            return Ok(Some(uuid.to_string()));
        }
    }
    Ok(None)
}

/// All session ids in this cwd, newest first. Used by the
/// REPL's `/resume` slash command to surface what's available.
#[must_use]
pub fn list_sessions(cwd: &Path) -> Vec<(String, String, std::time::SystemTime)> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(escape_cwd(cwd));
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String, std::time::SystemTime)> = Vec::new();
    for entry in read_dir.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() == 0 {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            let name = load_session_name(&dir, stem);
            out.push((stem.to_string(), name, mtime));
        }
    }
    out.sort_by_key(|(_, _, t)| std::cmp::Reverse(*t));
    out
}

impl Session {
    /// Persist a human-readable name for this session in a `.name`
    /// sidecar file next to the transcript.
    pub fn rename(&self, new_name: &str) -> Result<(), SessionError> {
        let path = self.project_dir.join(format!("{}.name", self.id));
        std::fs::write(&path, new_name)?;
        *self.name.lock().expect("name mutex") = new_name.to_string();
        Ok(())
    }

    /// Return the current name (empty string = unnamed).
    pub fn session_name(&self) -> String {
        self.name.lock().expect("name mutex").clone()
    }

    /// `~/.claude/projects/{escaped}/{session-id}.sock` — same dir
    /// as the transcript so anything that can find one can find the
    /// other.
    pub fn default_socket_path(&self) -> PathBuf {
        self.project_dir.join(format!("{}.sock", self.id))
    }

    /// Append one transcript entry. Writes are line-buffered + fsync
    /// so a crash mid-loop doesn't truncate the conversation.
    #[allow(clippy::significant_drop_tightening)]
    pub fn append(&self, entry: &Entry) -> Result<(), SessionError> {
        let json = serde_json::to_string(entry).expect("Entry is Serialize");
        {
            let mut f = self.file.lock().expect("transcript mutex poisoned");
            writeln!(f, "{json}")?;
            f.sync_all()?;
        }
        *self.last_uuid.lock().expect("last-uuid mutex") = Some(entry.uuid().to_string());
        Ok(())
    }

    /// Return the most recently appended message's uuid. Used to
    /// populate `parentUuid` on the next turn so the transcript forms
    /// a proper linked list, the way Claude Code does it.
    pub fn parent_uuid(&self) -> Option<String> {
        self.last_uuid.lock().expect("last-uuid mutex").clone()
    }

    pub fn now_iso() -> String {
        Timestamp::now().to_string()
    }
}

/// Replicates Claude Code's escaping rule (every non-ASCII-alphanumeric
/// byte → '-'). Kept identical to `tab_atelier::claude::escape_cwd` so
/// the two implementations agree on which directory to read/write.
fn escape_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// --- Transcript entry shapes ----------------------------------------------
//
// We only emit the fields the read side (`tab_atelier::claude::parse_messages`)
// actually consumes, plus enough metadata to reconstruct conversation
// state on resume. Claude Code emits more (file-history snapshots,
// permission mode changes, …) — those are optional, so leaving them
// off keeps the JSONL minimal and easy to read.

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum Entry {
    #[serde(rename = "user")]
    User {
        uuid: String,
        #[serde(rename = "parentUuid", skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: String,
        cwd: String,
        timestamp: String,
        message: UserMessage,
    },
    #[serde(rename = "assistant")]
    Assistant {
        uuid: String,
        #[serde(rename = "parentUuid", skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: String,
        cwd: String,
        timestamp: String,
        message: AssistantMessage,
    },
}

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum UserMessage {
    Plain { role: &'static str, content: String },
    Blocks { role: &'static str, content: Vec<Block> },
}

#[derive(Debug, Serialize, Clone)]
pub struct AssistantMessage {
    pub role: &'static str,
    pub model: String,
    pub content: Vec<Block>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

impl Entry {
    fn uuid(&self) -> &str {
        match self {
            Self::User { uuid, .. } | Self::Assistant { uuid, .. } => uuid,
        }
    }
}

#[must_use]
pub fn user_text(session: &Session, text: String) -> Entry {
    Entry::User {
        uuid: Uuid::new_v4().to_string(),
        parent_uuid: session.parent_uuid(),
        session_id: session.id.clone(),
        cwd: session.cwd.to_string_lossy().into_owned(),
        timestamp: Session::now_iso(),
        message: UserMessage::Plain {
            role: "user",
            content: text,
        },
    }
}

#[must_use]
pub fn tool_results(session: &Session, results: Vec<Block>) -> Entry {
    Entry::User {
        uuid: Uuid::new_v4().to_string(),
        parent_uuid: session.parent_uuid(),
        session_id: session.id.clone(),
        cwd: session.cwd.to_string_lossy().into_owned(),
        timestamp: Session::now_iso(),
        message: UserMessage::Blocks {
            role: "user",
            content: results,
        },
    }
}

#[must_use]
pub fn assistant_blocks(session: &Session, model: String, content: Vec<Block>) -> Entry {
    Entry::Assistant {
        uuid: Uuid::new_v4().to_string(),
        parent_uuid: session.parent_uuid(),
        session_id: session.id.clone(),
        cwd: session.cwd.to_string_lossy().into_owned(),
        timestamp: Session::now_iso(),
        message: AssistantMessage {
            role: "assistant",
            model,
            content,
        },
    }
}
