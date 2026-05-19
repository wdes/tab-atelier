// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session lifecycle + JSONL transcript writer.
//!
//! Catbus writes the same JSONL files Claude Code does, in the same
//! place (`~/.claude/projects/{escaped-cwd}/{session-id}.jsonl`).
//! That keeps the existing tab-atelier `/tabs/N/claude/messages`
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
    file: Mutex<File>,
    last_uuid: Mutex<Option<String>>,
}

/// Open (or resume) a session. Resuming just appends to the existing
/// JSONL; conversation history is reconstructed by re-reading the
/// file (Claude Code's behaviour).
pub fn open(cwd: &Path, resume_id: Option<&str>) -> Result<Session, SessionError> {
    let home = std::env::var_os("HOME").ok_or(SessionError::NoHome)?;
    let project_dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(escape_cwd(cwd));
    std::fs::create_dir_all(&project_dir)?;
    let id = resume_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_string);
    let transcript = project_dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&transcript)?;
    Ok(Session {
        id,
        cwd: cwd.to_path_buf(),
        project_dir,
        file: Mutex::new(file),
        last_uuid: Mutex::new(None),
    })
}

impl Session {
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
    Plain {
        role: &'static str,
        content: String,
    },
    Blocks {
        role: &'static str,
        content: Vec<Block>,
    },
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
