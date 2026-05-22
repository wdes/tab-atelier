// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The agent loop: send user message → call Messages API → execute
//! tool calls → loop until the model stops asking for tools.
//!
//! Three OAuth-specific quirks live here:
//!
//! * `Authorization: Bearer …` from `auth::access_token()`.
//! * `anthropic-beta: oauth-2025-04-20,claude-code-20250219` — the
//!   server rejects OAuth Messages requests without these flags.
//! * The first system block **must** start with the literal Claude
//!   Code identifier; without it the OAuth path returns a 4xx. Our
//!   own instructions go in a second system block.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::session::{self, Block, Session};
use crate::tools;

/// Identifier the server requires at the start of the first system
/// block on every OAuth-authenticated Messages call.
const CLAUDE_CODE_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Beta-flag list our OAuth tokens need. Server rejects requests
/// without `oauth-2025-04-20` + `claude-code-20250219` and the list
/// drifts; keep these in one place so it's obvious where to edit.
const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Sticking to a non-thinking, non-1M-context model keeps the bring-up
/// surface small. Both can be swapped via `/model` later.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Static portion of our second system block. The dynamic prefix (cwd,
/// plan-mode flag) is `format!()`'d once per call; this 1.5 KB tail is
/// a `&'static str` and no longer copied per round.
const SYSTEM_STATIC_INSTRUCTIONS: &str = "Your text replies are rendered directly in a terminal emulator \
    that supports ANSI colour and formatting — use ANSI SGR escapes \
    (bold, colours, etc.) to make output readable. Do NOT use \
    markdown — no asterisks, no backtick fences, no hashes. \
    Use ANSI instead: \x1b[1m for bold, \x1b[32m for green, \
    \x1b[33m for yellow, \x1b[31m for red, \x1b[36m for cyan, \
    \x1b[0m to reset.";

/// Session + history bundled together so swapping sessions mid-REPL
/// is atomic — we hold one write-lock and replace both at once.
struct ActiveSession {
    session: Arc<Session>,
    history: Vec<ApiMessage>,
}

pub struct Agent {
    auth: Auth,
    http: reqwest::Client,
    active: tokio::sync::RwLock<ActiveSession>,
    /// Plan-mode flag. When on, write/edit/bash refuse and tell the
    /// model to propose instead — same shape as Claude Code's
    /// shift-tab toggle.
    plan_mode: std::sync::atomic::AtomicBool,
    /// Current activity description shown in the REPL spinner.
    /// `None` = idle, `Some(s)` = description of what's happening.
    pub status: std::sync::Mutex<Option<String>>,
    /// Cumulative tokens consumed across all turns in this process.
    /// Both counters accumulate monotonically and are never reset.
    pub tokens_in: std::sync::atomic::AtomicU64,
    pub tokens_out: std::sync::atomic::AtomicU64,
    /// Cancellation flag for the currently-running turn. Re-built at
    /// the start of every `run_user_prompt` so Ctrl+C only kills the
    /// in-flight request, not future ones.
    cancel: std::sync::Mutex<CancellationToken>,
}

impl Agent {
    #[must_use]
    pub fn new(auth: Auth, session: Session) -> Self {
        Self {
            auth,
            http: reqwest::Client::builder()
                .user_agent("catbus-agent/0.1 (tab-atelier)")
                .build()
                .expect("http client init"),
            active: tokio::sync::RwLock::new(ActiveSession {
                session: Arc::new(session),
                history: Vec::new(),
            }),
            plan_mode: std::sync::atomic::AtomicBool::new(false),
            status: std::sync::Mutex::new(None),
            tokens_in: std::sync::atomic::AtomicU64::new(0),
            tokens_out: std::sync::atomic::AtomicU64::new(0),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
        }
    }

    /// Trip the cancellation token for the in-flight turn (if any).
    /// Safe to call when nothing is running — the next `run_user_prompt`
    /// installs a fresh token before doing any work.
    pub fn cancel_current(&self) {
        self.cancel.lock().expect("cancel mutex").cancel();
    }

    pub fn set_plan_mode(&self, on: bool) {
        self.plan_mode.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Current session id — cheap to read, no lock held.
    pub async fn session_id(&self) -> String {
        self.active.read().await.session.id.clone()
    }

    /// Current session cwd.
    #[allow(dead_code)]
    pub async fn session_cwd(&self) -> std::path::PathBuf {
        self.active.read().await.session.cwd.clone()
    }

    /// Transcript path for the current session — used to print the
    /// resume preview.
    pub async fn transcript_path(&self) -> std::path::PathBuf {
        self.active.read().await.session.transcript_path()
    }

    /// Arc to the active session — used to save token sidecars after each turn.
    pub async fn active_session(&self) -> Arc<Session> {
        Arc::clone(&self.active.read().await.session)
    }

    /// Current session name (empty = unnamed).
    pub async fn session_name(&self) -> String {
        self.active.read().await.session.session_name()
    }

    /// Rename the current session.
    pub async fn rename_session(&self, name: &str) -> Result<(), crate::session::SessionError> {
        self.active.read().await.session.rename(name)
    }

    /// Swap in a different session. Rebuilds the in-memory history
    /// from the new transcript so the model has full context.
    pub async fn swap_session(&self, new_session: Session) -> Result<(), AgentError> {
        let history = rebuild_history(&new_session.project_dir, &new_session.id);
        *self.active.write().await = ActiveSession {
            session: Arc::new(new_session),
            history,
        };
        Ok(())
    }

    /// One full turn: append the user's text to the transcript, then
    /// drive the tool loop until the assistant stops asking for
    /// tools. Returns the model's final assistant text concatenated.
    pub async fn run_user_prompt(&self, text: String) -> Result<String, AgentError> {
        // Fresh token per turn so a stale cancel doesn't kill the next
        // request before it even starts. Hold the lock only long enough
        // to swap; the inner future borrows the new clone.
        let token = {
            let mut slot = self.cancel.lock().expect("cancel mutex");
            *slot = CancellationToken::new();
            slot.clone()
        };
        *self.status.lock().expect("status mutex") = Some("thinking".into());
        let result = self.run_user_prompt_inner(text, &token).await;
        *self.status.lock().expect("status mutex") = None;
        result
    }

    #[allow(clippy::too_many_lines)]
    async fn run_user_prompt_inner(&self, text: String, cancel: &CancellationToken) -> Result<String, AgentError> {
        // Snapshot session Arc so we're not holding the RwLock across
        // await points in the tool loop.
        let session = Arc::clone(&self.active.read().await.session);

        // Persist + remember the user turn.
        let entry = session::user_text(&session, text.clone());
        session.append(&entry)?;
        {
            let mut active = self.active.write().await;
            active.history.push(ApiMessage {
                role: "user".into(),
                content: ApiContent::Plain(text),
            });
        }

        let mut final_text = String::new();
        // Cap on tool rounds. 200 is intentionally high — the model
        // self-terminates via end_turn long before this in normal use.
        // The env-var escape hatch exists for unusually long tasks.
        let max_rounds: u32 = std::env::var("CATBUS_MAX_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        for _ in 0..max_rounds {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            *self.status.lock().expect("status mutex") = Some("thinking".into());
            let resp = tokio::select! {
                res = self.call_messages() => res?,
                () = cancel.cancelled() => return Err(AgentError::Cancelled),
            };
            // Accumulate token usage immediately so the sidecar file
            // written after each prompt is always up to date.
            self.tokens_in
                .fetch_add(resp.usage.input_tokens, std::sync::atomic::Ordering::Relaxed);
            self.tokens_out
                .fetch_add(resp.usage.output_tokens, std::sync::atomic::Ordering::Relaxed);
            // Persist what the model produced first (clones the blocks
            // into the transcript entry — one clone), then iterate by
            // borrow, then move into in-memory history (no second clone).
            let entry = session::assistant_blocks(&session, resp.model.clone(), resp.content.clone());
            session.append(&entry)?;

            // Collect tool_use blocks by reference; pull any text into
            // the visible answer so the caller has *something* even
            // mid-tool-use. The borrows into `resp.content` survive the
            // tool dispatch awaits below — the history push that moves
            // `resp.content` happens *after* this borrow goes out of scope.
            let mut tool_uses: Vec<(&str, &str, &serde_json::Value)> = Vec::new();
            for block in &resp.content {
                match block {
                    Block::Text { text } => {
                        if !final_text.is_empty() {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                    }
                    Block::ToolUse { id, name, input } => {
                        tool_uses.push((id.as_str(), name.as_str(), input));
                    }
                    Block::ToolResult { .. } => {}
                }
            }

            let stop_reason = resp.stop_reason.clone();
            if matches!(stop_reason.as_deref(), Some("end_turn" | "stop_sequence")) || tool_uses.is_empty() {
                // No tool work to do — end the borrow into resp.content and
                // move it straight into history. Inner scope keeps the
                // write-lock guard tight (clippy::significant_drop_tightening).
                let _ = tool_uses;
                {
                    let mut active = self.active.write().await;
                    active.history.push(ApiMessage {
                        role: "assistant".into(),
                        content: ApiContent::Blocks(resp.content),
                    });
                }
                return Ok(final_text);
            }

            // Run tools, build a single user-message of tool_result
            // blocks (Messages API wants them all in one message,
            // in the same order the model produced the tool_use
            // blocks).
            let plan = self.plan_mode.load(std::sync::atomic::Ordering::Relaxed);
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in &tool_uses {
                if cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                // Show the tool name (and a short input summary for Bash)
                // in the status so the spinner reflects what's running.
                let label = tool_status_label(name, input);
                *self.status.lock().expect("status mutex") = Some(label);
                let (content, is_error) = tokio::select! {
                    out = tools::dispatch(name, input, &session.cwd, plan) => {
                        out.map_or_else(|e| (format!("Error: {e}"), true), |out| (out, false))
                    }
                    () = cancel.cancelled() => return Err(AgentError::Cancelled),
                };
                results.push(Block::ToolResult {
                    tool_use_id: (*id).to_string(),
                    content,
                    is_error,
                });
            }
            // Done with the borrows — move resp.content into history now.
            let _ = tool_uses;
            {
                let mut active = self.active.write().await;
                active.history.push(ApiMessage {
                    role: "assistant".into(),
                    content: ApiContent::Blocks(resp.content),
                });
            }
            let entry = session::tool_results(&session, results.clone());
            session.append(&entry)?;
            {
                let mut active = self.active.write().await;
                active.history.push(ApiMessage {
                    role: "user".into(),
                    content: ApiContent::Blocks(results),
                });
            }
        }
        // 32 rounds exhausted. Return whatever text was collected so far
        // so the REPL shows it, and append a warning so the user knows
        // the loop was cut short rather than silently losing output.
        if final_text.is_empty() {
            Err(AgentError::TooManyRounds)
        } else {
            final_text.push_str("\n\n\x1b[33m[tool loop hit the 32-round cap — response may be incomplete]\x1b[0m");
            Ok(final_text)
        }
    }

    async fn call_messages(&self) -> Result<MessagesResp, AgentError> {
        let token = self.auth.access_token().await.map_err(AgentError::Auth)?;
        let active = self.active.read().await;
        let cwd = active.session.cwd.display().to_string();
        let tool_specs = tools::tool_specs();
        // Hold the read lock across .json(&body) so MessagesReq can
        // borrow `&active.history` instead of cloning the full Vec.
        // reqwest serializes the body synchronously in .json(), so the
        // lock is released right after that call returns.
        let body = MessagesReq {
            model: DEFAULT_MODEL,
            max_tokens: 8192,
            system: vec![
                SystemBlock {
                    kind: "text",
                    text: std::borrow::Cow::Borrowed(CLAUDE_CODE_PREFIX),
                },
                SystemBlock {
                    kind: "text",
                    text: std::borrow::Cow::Owned(format!(
                        "You are operating as `catbus-agent` inside tab-atelier. \
                         Current working directory: {cwd}. \
                         Plan-mode is {plan}.",
                        plan = if self.plan_mode.load(std::sync::atomic::Ordering::Relaxed) {
                            "ON — propose changes, do not execute write/edit/bash."
                        } else {
                            "off"
                        },
                    )),
                },
                SystemBlock {
                    kind: "text",
                    text: std::borrow::Cow::Borrowed(SYSTEM_STATIC_INSTRUCTIONS),
                },
            ],
            tools: &tool_specs,
            messages: &active.history,
        };
        let request = self
            .http
            .post(MESSAGES_URL)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", ANTHROPIC_BETA)
            .json(&body);
        drop(active);
        let resp = request
            .send()
            .await
            .map_err(|e| AgentError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Api(format!("{status}: {body}")));
        }
        resp.json::<MessagesResp>()
            .await
            .map_err(|e| AgentError::Http(format!("decode: {e}")))
    }
}

/// Reconstruct the conversation history from a JSONL transcript.
/// Used when resuming a session in-place so the model has full context.
/// Only user/assistant turns are loaded; tool results are part of user turns.
fn rebuild_history(project_dir: &std::path::Path, id: &str) -> Vec<ApiMessage> {
    use crate::session::Block;
    use std::io::BufRead;
    let path = project_dir.join(format!("{id}.jsonl"));
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let role = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        // We hold `v` mutably so we can `mem::take` the content sub-tree
        // instead of cloning it before `from_value`. The owned `v` is
        // dropped at the end of the loop iteration anyway.
        let Some(msg) = v.get_mut("message") else { continue };
        let Some(content_val) = msg.get_mut("content") else { continue };
        let content_owned = std::mem::take(content_val);
        match role {
            "user" => {
                let content = match content_owned {
                    serde_json::Value::String(s) => ApiContent::Plain(s),
                    arr @ serde_json::Value::Array(_) => {
                        let blocks: Vec<Block> = serde_json::from_value(arr).unwrap_or_default();
                        ApiContent::Blocks(blocks)
                    }
                    _ => continue,
                };
                out.push(ApiMessage {
                    role: "user".into(),
                    content,
                });
            }
            "assistant" => {
                let blocks: Vec<Block> = serde_json::from_value(content_owned).unwrap_or_default();
                if !blocks.is_empty() {
                    out.push(ApiMessage {
                        role: "assistant".into(),
                        content: ApiContent::Blocks(blocks),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Build a short human-readable label for the spinner while a tool runs.
/// For Bash we include the first 60 chars of the command so it's clear
/// what's executing; for Read/Write/Edit we show the filename.
fn tool_status_label(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let short: String = cmd.chars().take(60).collect();
            let ellipsis = if cmd.len() > 60 { "…" } else { "" };
            format!("Bash: {short}{ellipsis}")
        }
        "Read" | "Write" | "Edit" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            // Show only the last two components so long paths don't overflow.
            // `file_name()` + `parent().and_then(|p| p.file_name())` gets us
            // both in O(1) without the previous quadruple-collect dance.
            let p = std::path::Path::new(path);
            let short = match (p.file_name(), p.parent().and_then(|par| par.file_name())) {
                (Some(file), Some(parent)) => format!("{}/{}", parent.to_string_lossy(), file.to_string_lossy()),
                (Some(file), None) => file.to_string_lossy().into_owned(),
                _ => path.to_string(),
            };
            format!("{name}: {short}")
        }
        "Delegate" => {
            let target = input.get("target").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Delegate → {target}")
        }
        other => other.to_string(),
    }
}

// --- API wire types --------------------------------------------------------

#[derive(Serialize)]
struct MessagesReq<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    tools: &'a [serde_json::Value],
    messages: &'a [ApiMessage],
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: std::borrow::Cow<'a, str>,
}

#[derive(Deserialize, Debug, Clone)]
struct MessagesResp {
    content: Vec<Block>,
    model: String,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Serialize, Clone)]
struct ApiMessage {
    role: String,
    content: ApiContent,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum ApiContent {
    Plain(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("auth: {0}")]
    Auth(crate::auth::AuthError),
    #[error("api: {0}")]
    Api(String),
    #[error("http: {0}")]
    Http(String),
    #[error("transcript: {0}")]
    Transcript(#[from] crate::session::SessionError),
    #[error("tool loop exceeded the round cap (set CATBUS_MAX_ROUNDS to raise it)")]
    TooManyRounds,
    #[error("cancelled by user")]
    Cancelled,
}
