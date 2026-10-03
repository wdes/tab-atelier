// SPDX-License-Identifier: MPL-2.0

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
/// `_new_session` is retained because callers pass it — the CLI flag and the `Spawn` tool both name
/// a fresh session explicitly — but it no longer decides anything: **a fresh session is the default
/// now.** It used to be the other way round, and the surprise was real: a tab reopened continued
/// whichever conversation was last in that directory, and two agents in one directory silently shared
/// a history. Continuing an earlier session is only ever explicit, through `resume_id`.
pub fn open(cwd: &Path, resume_id: Option<&str>, _new_session: bool) -> Result<Session, SessionError> {
    let home = std::env::var_os("HOME").ok_or(SessionError::NoHome)?;
    let project_dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(escape_cwd(cwd));
    std::fs::create_dir_all(&project_dir)?;
    // An explicit id is the only way to continue an earlier session, and it wins even when
    // `--new-session` is also passed: a caller that names a session means that session, and every
    // existing caller passes `--new-session` as a matter of course. Anything else is fresh.
    let id = resume_id.map_or_else(|| Uuid::new_v4().to_string(), ToString::to_string);
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

/// Path of the `.gate` sidecar: the permission mode as it was last set.
fn gate_sidecar(project_dir: &Path, id: &str) -> PathBuf {
    project_dir.join(format!("{id}.gate"))
}

/// Path of the `.model` sidecar: the model chosen for this session.
fn model_sidecar(project_dir: &Path, id: &str) -> PathBuf {
    project_dir.join(format!("{id}.model"))
}

/// One readable message from a transcript, for the resume tail.
///
/// A *message*, not a paired exchange: the last thing in a transcript can be a
/// prompt with no reply yet — a session closed or killed between the two — and
/// that prompt is exactly what says where the operator left off. An
/// exchange-shaped reader drops it, because it has nothing to pair it with.
#[derive(Debug)]
pub struct Message {
    /// True for a prompt the operator typed; false for an assistant turn, which
    /// may be a tool-call summary rather than prose.
    pub user: bool,
    pub text: String,
}

/// Longest assistant text block kept in the preview, in characters.
///
/// The tail is for orientation, not for reading the conversation back: the full
/// reply is in the transcript, and the banner only needs enough of it to say
/// what the session was last doing.
const ASSISTANT_PREVIEW_CHARS: usize = 200;

/// Return up to `n` most-recent messages from the transcript at `path`.
///
/// Only text is surfaced — tool calls are collapsed to a one-line count and tool
/// results are skipped, so the preview stays readable. Lines that do not parse,
/// or that hold a shape this build does not know (a newer writer's entry), are
/// passed over rather than ending the read.
#[must_use]
pub fn last_messages(path: &Path, n: usize) -> Vec<Message> {
    use std::collections::VecDeque;
    use std::io::BufRead;
    if n == 0 {
        return Vec::new();
    }
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    // A ring of the last `n`, so the whole transcript is never held in memory.
    // This is called at every banner draw, and a session can be thousands of
    // turns long.
    let mut kept: VecDeque<Message> = VecDeque::with_capacity(n + 1);
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(message) = message_of(&v) else { continue };
        kept.push_back(message);
        if kept.len() > n {
            kept.pop_front();
        }
    }
    kept.into_iter().collect()
}

/// The readable message in one transcript line, or `None` when the line is not
/// one — a tool result, a meta entry, or anything a newer writer adds.
fn message_of(v: &serde_json::Value) -> Option<Message> {
    let role = v.get("type")?.as_str()?;
    let msg = v.get("message")?;
    match role {
        // Plain string content = a real user prompt. Array content = tool
        // results, which are the model's own bookkeeping rather than something
        // the operator typed, so they are skipped.
        "user" => {
            let text = msg.get("content")?.as_str()?.trim();
            (!text.is_empty()).then(|| Message {
                user: true,
                text: text.to_owned(),
            })
        }
        "assistant" => {
            let blocks = msg.get("content")?.as_array()?;
            let mut parts: Vec<String> = Vec::new();
            let mut tool_count = 0usize;
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                parts.push(trimmed.chars().take(ASSISTANT_PREVIEW_CHARS).collect());
                            }
                        }
                    }
                    Some("tool_use") => tool_count += 1,
                    _ => {}
                }
            }
            if tool_count > 0 {
                // Plain text, not an escape sequence: the banner prints through
                // `Ui::print_above`, which writes every character into a buffer
                // cell, so an SGR here would be stored as bytes and shown as
                // litter rather than dimming anything.
                parts.push(format!(
                    "[{tool_count} tool call{}]",
                    if tool_count == 1 { "" } else { "s" }
                ));
            }
            (!parts.is_empty()).then(|| Message {
                user: false,
                text: parts.join(" "),
            })
        }
        _ => None,
    }
}

/// Read the last non-empty JSONL line of `path` and pluck its
/// `uuid`. Used to chain `parentUuid` correctly on resume.
///
/// Seeks backwards from EOF in `READ_CHUNK`-byte chunks until the buffer
/// contains at least two newlines (or the file's start is reached), so we
/// only touch the tail of the transcript instead of `read_to_string`-ing
/// the entire (potentially MB-sized) file every session-open.
fn last_entry_uuid(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    const READ_CHUNK: u64 = 4096;
    let mut file = std::fs::File::open(path)?;
    let total = file.seek(SeekFrom::End(0))?;
    if total == 0 {
        return Ok(None);
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = total;
    // Grow the tail buffer backwards until we've captured at least one
    // line *before* the trailing newline at EOF, or we've slurped the
    // whole file. The 2-newline criterion guards against bailing out
    // when only the EOF newline is in view.
    loop {
        let want = READ_CHUNK.min(pos);
        pos -= want;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; want as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        // Two-newline criterion guarantees at least one complete line
        // *before* the trailing EOF newline is in view.
        let mut found = 0usize;
        for &b in &buf {
            if b == b'\n' {
                found += 1;
                if found >= 2 {
                    break;
                }
            }
        }
        if found >= 2 || pos == 0 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
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

/// First non-empty user prompt in a session, truncated to `max_chars`,
/// stripped of newlines. Used by `/resume` as a fallback label when
/// the session has no explicit `/rename`-set name yet — gives the
/// user *something* recognisable instead of a bare UUID.
#[must_use]
pub fn first_prompt(path: &Path, max_chars: usize) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // Plain string = a real user prompt. Array content = tool
        // results, which we skip — they aren't what the human typed.
        let Some(serde_json::Value::String(s)) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = one_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let label: String = trimmed.chars().take(max_chars).collect();
        return Some(if trimmed.chars().count() > max_chars {
            format!("{label}…")
        } else {
            label
        });
    }
    None
}

/// `~/.claude/projects/<escaped-cwd>` — where session transcripts and
/// sidecars for `cwd` live. Returns `None` if `HOME` isn't set.
#[must_use]
pub fn project_dir_for(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(escape_cwd(cwd)),
    )
}

/// All session ids in this cwd, newest first. Used by the
/// REPL's `/resume` slash command to surface what's available.
#[must_use]
pub fn list_sessions(cwd: &Path) -> Vec<(String, String, std::time::SystemTime)> {
    let Some(dir) = project_dir_for(cwd) else {
        return Vec::new();
    };
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
            // A session's label: its `/rename`d name, or failing that the opening
            // prompt. A bare UUID in a listing is not something anyone recognises,
            // and the first thing that was asked is what the operator remembers —
            // which is what `first_prompt` exists for.
            let name = load_session_name(&dir, stem);
            let label = if name.is_empty() {
                first_prompt(&p, 60).unwrap_or_default()
            } else {
                name
            };
            out.push((stem.to_string(), label, mtime));
        }
    }
    out.sort_by_key(|(_, _, t)| std::cmp::Reverse(*t));
    out
}

impl Session {
    /// Persist cumulative token usage to a `{id}.tokens.json` sidecar
    /// alongside the transcript. Written after every prompt so
    /// tab-atelier can poll it from the session's project dir.
    /// What this session has already spent, read back from its sidecar.
    ///
    /// `None` for a fresh session, or for one whose sidecar is missing or unreadable — every
    /// caller treats that as "nothing spent yet", which is the right default for a file that
    /// only ever adds to itself.
    ///
    /// This is what makes a resume a continuation rather than a restart: without it, reopening a
    /// session showed it having spent nothing, and the running totals in the status lines
    /// started again from zero while the transcript behind them said otherwise.
    #[must_use]
    pub fn load_tokens(&self) -> Option<serde_json::Value> {
        let path = self.project_dir.join(format!("{}.tokens.json", self.id));
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `input` and `output` stay at the top level because tab-atelier reads them there, and its
    /// reader defaults a missing key to zero — so the grouped cost rides *beside* them in a
    /// `cost` object rather than replacing them. A reader that does not know about `cost` sees
    /// exactly the file it saw before, and one that does gets the amounts per currency.
    pub fn save_tokens(&self, input: u64, output: u64, cost: &serde_json::Value) -> Result<(), SessionError> {
        let path = self.project_dir.join(format!("{}.tokens.json", self.id));
        let json = serde_json::to_string(&serde_json::json!({
            "input": input,
            "output": output,
            "cost": cost,
        }))
        .expect("token JSON is always valid");
        std::fs::write(&path, json)?;
        Ok(())
    }

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

    /// The permission mode this session was last left in, if it was ever set.
    ///
    /// Recorded beside the transcript the way the name is, because the mode is a
    /// decision about *this* session and has to outlive the process. Tab Atelier
    /// restarts the agent on a tab reopen, so a mode held only in memory is lost
    /// exactly when the operator comes back to check it worked — which reads as
    /// the mode never having been set. An unrecognised or unreadable file is
    /// `None` rather than an error: a stale sidecar must not stop the agent from
    /// starting, and the fallback is the same as having no file at all.
    #[must_use]
    pub fn saved_gate(&self) -> Option<crate::tools::Gate> {
        let raw = std::fs::read_to_string(gate_sidecar(&self.project_dir, &self.id)).ok()?;
        crate::tools::parse_gate(&raw)
    }

    /// Remember the permission mode for this session. See [`Self::saved_gate`].
    pub fn save_gate(&self, gate: crate::tools::Gate) -> Result<(), SessionError> {
        std::fs::write(gate_sidecar(&self.project_dir, &self.id), gate.as_str())?;
        Ok(())
    }

    /// The model chosen for this session, if one ever was.
    ///
    /// A sidecar rather than something derived, and that is the interesting part:
    /// the transcript *does* record a model on every assistant turn, but it records
    /// the one the **relay served**, not the one the client asked for — a live
    /// session's transcript says `deepseek-flash` on all 2756 turns, because the
    /// proxy rewrites the field. Re-asking for that name would be a different
    /// request than the one that produced it, so the transcript record is only good
    /// for deciding what is answering (see [`Self::last_model`]), not for deciding
    /// what to ask.
    ///
    /// An unreadable or unrecognised file is `None` rather than an error, for the
    /// same reason as [`Self::saved_gate`]: a stale sidecar must not stop the agent.
    #[must_use]
    pub fn saved_model(&self) -> Option<String> {
        let raw = std::fs::read_to_string(model_sidecar(&self.project_dir, &self.id)).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Remember the model for this session. See [`Self::saved_model`].
    pub fn save_model(&self, model: &str) -> Result<(), SessionError> {
        std::fs::write(model_sidecar(&self.project_dir, &self.id), model.trim())?;
        Ok(())
    }

    /// Path to the JSONL transcript file for this session.
    pub fn transcript_path(&self) -> PathBuf {
        self.project_dir.join(format!("{}.jsonl", self.id))
    }

    /// `~/.claude/projects/{escaped}/{session-id}.sock` — same dir
    /// as the transcript so anything that can find one can find the
    /// other.
    pub fn default_socket_path(&self) -> PathBuf {
        self.project_dir.join(format!("{}.sock", self.id))
    }

    /// The model this session was last served, read back from its transcript.
    ///
    /// Claude Code records the model on each assistant turn and continues with it,
    /// and this is the same idea: the model is a property of the *session*, not of
    /// one run of the process, so it does not belong in a flag that has to be
    /// passed again on every resume. Tab Atelier restarts the agent whenever a tab
    /// is reopened, which is exactly when a setting held only in memory is lost.
    ///
    /// Knowing it before the first reply also has a second use: whether to send the
    /// built-in identity line is decided from the model, and a resumed session can
    /// therefore get that right on its very first request instead of after a round
    /// trip. See [`crate::identity`].
    ///
    /// The last non-empty value wins, so a session that changed model mid-way
    /// carries on with the newer one. An unreadable or absent transcript yields
    /// `None`, which the caller treats as "not known" — the same as a new session.
    #[must_use]
    pub fn last_model(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.transcript_path()).ok()?;
        let mut found = None;
        for line in raw.lines() {
            // Parsed as a `Value` rather than an `Entry`: a transcript may hold
            // entries this build does not model (written by a newer one, or by
            // Claude Code itself), and one of those must not hide the model.
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let message = &entry["message"];
            if message["role"] != "assistant" {
                continue;
            }
            if let Some(model) = message["model"].as_str().filter(|m| !m.trim().is_empty()) {
                found = Some(model.to_owned());
            }
        }
        found
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
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// A reasoning block from a thinking model.
    ///
    /// The relay may route a session to a model that emits these, and the
    /// transcript we send back has to be the one we received — so the parts
    /// are kept verbatim and never interpreted. Only surface the *visible*
    /// text to the user; `thinking` is display-only for the operator and
    /// nothing downstream should treat it as an answer.
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        /// Opaque marker binding this block to the response it came from.
        /// Round-tripped unchanged; never inspected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    /// Write a transcript of `json!` lines and hand back the path.
    fn transcript(lines: &[serde_json::Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let body = lines.iter().fold(String::new(), |mut body, line| {
            let _ = writeln!(body, "{line}");
            body
        });
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    fn user(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    fn assistant(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "text", "text": text}]}})
    }

    /// The tail keeps a prompt that was never answered.
    ///
    /// This is the whole reason the reader returns messages rather than paired
    /// exchanges: a session closed or killed between the prompt and the reply ends
    /// on a lone user turn, and that turn is exactly what says where the operator
    /// left off. Pairing dropped it, so a resumed session looked as though it had
    /// ended with the answer *before* the last thing asked.
    #[test]
    fn the_tail_keeps_a_prompt_that_was_never_answered() {
        let (_dir, path) = transcript(&[user("first"), assistant("done"), user("and now this")]);
        let tail = last_messages(&path, 10);
        assert_eq!(tail.len(), 3);
        assert!(tail[2].user, "the last message is the unanswered prompt");
        assert_eq!(tail[2].text, "and now this");
    }

    /// Only the last `n` are returned, oldest first, so the caller prints them in
    /// the order they happened.
    #[test]
    fn only_the_last_n_messages_are_kept() {
        let (_dir, path) = transcript(&[user("one"), assistant("two"), user("three"), assistant("four")]);
        let tail = last_messages(&path, 2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].text, "three");
        assert_eq!(tail[1].text, "four");
        assert!(tail[0].user, "the pair kept is the newest one, not the oldest");
        assert!(!tail[1].user);
    }

    /// Tool results are the model's own bookkeeping rather than something the
    /// operator wrote, and a tool call is one plain line — the banner prints into
    /// buffer cells, where an SGR escape would be stored as litter rather than
    /// dimming anything.
    #[test]
    fn tool_traffic_is_summarised_in_plain_text() {
        let tool_result = serde_json::json!({"type": "user", "message": {"role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "the file"}]}});
        let tool_use = serde_json::json!({"type": "assistant", "message": {"role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]}});
        let (_dir, path) = transcript(&[user("go"), tool_use, tool_result, assistant("all done")]);
        let tail = last_messages(&path, 10);
        assert_eq!(tail.len(), 3, "the tool result is not a message of its own");
        assert_eq!(tail[1].text, "[1 tool call]");
        assert!(!tail[1].text.contains('\u{1b}'), "no escape bytes in a banner line");
        assert_eq!(tail[2].text, "all done");
    }

    /// A long reply is truncated so the banner stays a summary; the transcript
    /// keeps all of it.
    #[test]
    fn a_long_reply_is_truncated_for_the_preview() {
        let long = "x".repeat(ASSISTANT_PREVIEW_CHARS * 3);
        let (_dir, path) = transcript(&[assistant(&long)]);
        let tail = last_messages(&path, 10);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text.chars().count(), ASSISTANT_PREVIEW_CHARS);
    }

    /// An absent transcript, and a request for none, are both empty rather than an
    /// error: the banner is decoration, and its absence must not stop a session
    /// from starting.
    #[test]
    fn an_absent_transcript_yields_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        assert!(last_messages(&dir.path().join("absent.jsonl"), 10).is_empty());
        let (_dir, path) = transcript(&[user("hi")]);
        assert!(last_messages(&path, 0).is_empty());
    }

    /// A transcript the agent wrote must be one it can read back.
    ///
    /// `is_error` is skipped when false, so the common `tool_result` on disk has
    /// no such key — and without `serde(default)` the reader rejected every one
    /// of them. The damage was not limited to the missing result: the `tool_use`
    /// above it was then an orphan, so `truncate_at_orphan_tool_use` dropped
    /// that turn too, and resuming a session silently lost its tool history.
    #[test]
    fn a_tool_result_written_without_is_error_reads_back() {
        let written = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "t1",
            "content": "the file",
        });
        let block: Block = serde_json::from_value(written.clone()).expect("reads back");

        assert_eq!(
            serde_json::to_value(&block).expect("writes"),
            written,
            "and it is still written the same terse way it was read"
        );
    }

    /// An error result round-trips too, with the flag intact.
    #[test]
    fn an_error_tool_result_keeps_is_error() {
        let block = Block::ToolResult {
            tool_use_id: "t1".into(),
            content: "boom".into(),
            is_error: true,
        };
        let json = serde_json::to_value(&block).expect("writes");
        assert_eq!(json["is_error"], true);
        let read_back: Block = serde_json::from_value(json).expect("reads");
        assert!(matches!(read_back, Block::ToolResult { is_error: true, .. }));
    }

    /// The mode is remembered beside the transcript, and a session that was never
    /// set has none.
    ///
    /// This is what makes `/auto` survive a tab reopen: Tab Atelier restarts the
    /// agent, so a mode held only in memory is gone exactly when the operator
    /// comes back to check it — which reads as the mode never having been set at
    /// all.
    #[test]
    fn a_saved_gate_is_read_back_and_absent_before_it_is_set() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing written yet: no opinion, so the caller's default stands.
        assert_eq!(
            std::fs::read_to_string(gate_sidecar(dir.path(), "s1")).ok(),
            None,
            "no sidecar should exist before anything is saved"
        );

        for gate in [
            crate::tools::Gate::Auto,
            crate::tools::Gate::Plan,
            crate::tools::Gate::Open,
        ] {
            std::fs::write(gate_sidecar(dir.path(), "s1"), gate.as_str()).unwrap();
            assert_eq!(
                crate::tools::parse_gate(&std::fs::read_to_string(gate_sidecar(dir.path(), "s1")).unwrap()),
                Some(gate),
                "{} must round-trip through its own spelling",
                gate.as_str()
            );
        }
    }

    /// A damaged or unknown sidecar must not stop the agent from starting: the
    /// caller falls back to its default, which is the same as having no file.
    #[test]
    fn an_unreadable_gate_sidecar_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(gate_sidecar(dir.path(), "s2"), "banana").unwrap();
        assert_eq!(crate::tools::parse_gate("banana"), None);
        // Trailing whitespace, as a hand-edited file would have.
        std::fs::write(gate_sidecar(dir.path(), "s3"), "auto\n").unwrap();
        assert_eq!(
            crate::tools::parse_gate(&std::fs::read_to_string(gate_sidecar(dir.path(), "s3")).unwrap()),
            Some(crate::tools::Gate::Auto),
            "parse_gate trims, so a newline is not a corruption"
        );
    }

    /// The model sidecar round-trips, and an absent or blank one is no opinion.
    ///
    /// A sidecar rather than something read from the transcript, and the reason is
    /// in `saved_model`'s doc: the transcript records the model the *relay served*
    /// (a live one says `deepseek-flash` on every turn, because the proxy rewrites
    /// the field), which is not the name to ask for again.
    #[test]
    fn a_saved_model_round_trips_and_blank_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = model_sidecar(dir.path(), "s1");
        assert_eq!(std::fs::read_to_string(&path).ok(), None, "nothing saved yet");

        std::fs::write(&path, "claude-opus-4\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).map(|raw| raw.trim().to_owned()).ok(),
            Some("claude-opus-4".to_owned()),
            "a trailing newline must not be part of the name"
        );

        // Blank is "no opinion", not a model named "".
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).map(|raw| raw.trim().to_owned()).ok(),
            Some(String::new())
        );
    }

    /// The three sidecars a session keeps are distinct files, and each is per
    /// session. A shared name would let one session's model leak into another's.
    #[test]
    fn each_sidecar_is_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let model = model_sidecar(dir.path(), "a");
        let gate = gate_sidecar(dir.path(), "a");
        assert_ne!(model, gate);
        assert_ne!(model_sidecar(dir.path(), "a"), model_sidecar(dir.path(), "b"));
        assert!(model.to_string_lossy().ends_with(".model"), "{}", model.display());
    }

    /// One session's mode is not another's.
    #[test]
    fn the_gate_sidecar_is_per_session() {
        let dir = tempfile::tempdir().unwrap();
        assert_ne!(gate_sidecar(dir.path(), "a"), gate_sidecar(dir.path(), "b"));
    }
}
