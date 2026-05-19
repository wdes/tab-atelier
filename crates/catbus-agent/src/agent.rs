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
    /// Set to `true` while the agent is running a prompt. The REPL
    /// polls this to drive its progress spinner.
    pub working: std::sync::atomic::AtomicBool,
    /// Cumulative tokens consumed across all turns in this process.
    /// Both counters accumulate monotonically and are never reset.
    pub tokens_in: std::sync::atomic::AtomicU64,
    pub tokens_out: std::sync::atomic::AtomicU64,
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
            working: std::sync::atomic::AtomicBool::new(false),
            tokens_in: std::sync::atomic::AtomicU64::new(0),
            tokens_out: std::sync::atomic::AtomicU64::new(0),
        }
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
        self.working.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = self.run_user_prompt_inner(text).await;
        self.working.store(false, std::sync::atomic::Ordering::Relaxed);
        result
    }

    async fn run_user_prompt_inner(&self, text: String) -> Result<String, AgentError> {
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
            let resp = self.call_messages().await?;
            // Accumulate token usage immediately so the sidecar file
            // written after each prompt is always up to date.
            self.tokens_in
                .fetch_add(resp.usage.input_tokens, std::sync::atomic::Ordering::Relaxed);
            self.tokens_out
                .fetch_add(resp.usage.output_tokens, std::sync::atomic::Ordering::Relaxed);
            // Persist + record what the model produced.
            let entry = session::assistant_blocks(&session, resp.model.clone(), resp.content.clone());
            session.append(&entry)?;
            {
                let mut active = self.active.write().await;
                active.history.push(ApiMessage {
                    role: "assistant".into(),
                    content: ApiContent::Blocks(resp.content.clone()),
                });
            }

            // Collect tool_use blocks; pull any text into the visible
            // answer so the caller has *something* even mid-tool-use.
            let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
            for block in &resp.content {
                match block {
                    Block::Text { text } => {
                        if !final_text.is_empty() {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                    }
                    Block::ToolUse { id, name, input } => {
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
                    }
                    Block::ToolResult { .. } => {}
                }
            }

            if matches!(resp.stop_reason.as_deref(), Some("end_turn" | "stop_sequence")) || tool_uses.is_empty() {
                return Ok(final_text);
            }

            // Run tools, build a single user-message of tool_result
            // blocks (Messages API wants them all in one message,
            // in the same order the model produced the tool_use
            // blocks).
            let plan = self.plan_mode.load(std::sync::atomic::Ordering::Relaxed);
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in tool_uses {
                let (content, is_error) = tools::dispatch(&name, &input, &session.cwd, plan)
                    .await
                    .map_or_else(|e| (format!("Error: {e}"), true), |out| (out, false));
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content,
                    is_error,
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
        let history = active.history.clone();
        let cwd = active.session.cwd.display().to_string();
        drop(active);
        let body = MessagesReq {
            model: DEFAULT_MODEL,
            max_tokens: 8192,
            system: vec![
                SystemBlock {
                    kind: "text",
                    text: CLAUDE_CODE_PREFIX.into(),
                },
                SystemBlock {
                    kind: "text",
                    text: format!(
                        "You are operating as `catbus-agent` inside tab-atelier. \
                         Current working directory: {cwd}. \
                         Plan-mode is {}.",
                        if self.plan_mode.load(std::sync::atomic::Ordering::Relaxed) {
                            "ON — propose changes, do not execute write/edit/bash."
                        } else {
                            "off"
                        },
                    ),
                },
            ],
            tools: tools::tool_specs(),
            messages: history,
        };
        let resp = self
            .http
            .post(MESSAGES_URL)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", ANTHROPIC_BETA)
            .json(&body)
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
    let path = project_dir.join(format!("{id}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(role) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(msg) = v.get("message") else { continue };
        match role {
            "user" => {
                let content = match msg.get("content") {
                    Some(serde_json::Value::String(s)) => ApiContent::Plain(s.clone()),
                    Some(serde_json::Value::Array(_)) => {
                        let blocks: Vec<Block> =
                            serde_json::from_value(msg["content"].clone()).unwrap_or_default();
                        ApiContent::Blocks(blocks)
                    }
                    _ => continue,
                };
                out.push(ApiMessage { role: "user".into(), content });
            }
            "assistant" => {
                let Some(content_val) = msg.get("content") else { continue };
                let blocks: Vec<Block> = serde_json::from_value(content_val.clone()).unwrap_or_default();
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

// --- API wire types --------------------------------------------------------

#[derive(Serialize)]
struct MessagesReq<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock>,
    tools: Vec<serde_json::Value>,
    messages: Vec<ApiMessage>,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
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
}
