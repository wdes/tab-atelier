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
const CLAUDE_CODE_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Beta-flag list our OAuth tokens need. Server rejects requests
/// without `oauth-2025-04-20` + `claude-code-20250219` and the list
/// drifts; keep these in one place so it's obvious where to edit.
const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Sticking to a non-thinking, non-1M-context model keeps the bring-up
/// surface small. Both can be swapped via `/model` later.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

pub struct Agent {
    auth: Auth,
    session: Arc<Session>,
    http: reqwest::Client,
    /// Conversation history in API-message form. Mirrors what we've
    /// written to the JSONL. Kept in memory so we don't re-parse the
    /// transcript on every turn.
    history: tokio::sync::Mutex<Vec<ApiMessage>>,
    /// Plan-mode flag. When on, write/edit/bash refuse and tell the
    /// model to propose instead — same shape as Claude Code's
    /// shift-tab toggle.
    plan_mode: std::sync::atomic::AtomicBool,
}

impl Agent {
    #[must_use]
    pub fn new(auth: Auth, session: Session) -> Self {
        Self {
            auth,
            session: Arc::new(session),
            http: reqwest::Client::builder()
                .user_agent("catbus-agent/0.1 (tab-atelier)")
                .build()
                .expect("http client init"),
            history: tokio::sync::Mutex::new(Vec::new()),
            plan_mode: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_plan_mode(&self, on: bool) {
        self.plan_mode
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// One full turn: append the user's text to the transcript, then
    /// drive the tool loop until the assistant stops asking for
    /// tools. Returns the model's final assistant text concatenated.
    pub async fn run_user_prompt(&self, text: String) -> Result<String, AgentError> {
        // Persist + remember the user turn.
        let entry = session::user_text(&self.session, text.clone());
        self.session.append(&entry)?;
        self.history.lock().await.push(ApiMessage {
            role: "user".into(),
            content: ApiContent::Plain(text),
        });

        let mut final_text = String::new();
        // Hard cap on tool rounds to avoid runaway loops if the
        // model keeps calling tools forever. 32 is generous —
        // typical interactions resolve in 1–6.
        for _ in 0..32 {
            let resp = self.call_messages().await?;
            // Persist + record what the model produced.
            let entry =
                session::assistant_blocks(&self.session, resp.model.clone(), resp.content.clone());
            self.session.append(&entry)?;
            self.history.lock().await.push(ApiMessage {
                role: "assistant".into(),
                content: ApiContent::Blocks(resp.content.clone()),
            });

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

            if matches!(resp.stop_reason.as_deref(), Some("end_turn" | "stop_sequence"))
                || tool_uses.is_empty()
            {
                return Ok(final_text);
            }

            // Run tools, build a single user-message of tool_result
            // blocks (Messages API wants them all in one message,
            // in the same order the model produced the tool_use
            // blocks).
            let plan = self.plan_mode.load(std::sync::atomic::Ordering::Relaxed);
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in tool_uses {
                let (content, is_error) = tools::dispatch(&name, &input, &self.session.cwd, plan)
                    .await
                    .map_or_else(
                        |e| (format!("Error: {e}"), true),
                        |out| (out, false),
                    );
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content,
                    is_error,
                });
            }
            let entry = session::tool_results(&self.session, results.clone());
            self.session.append(&entry)?;
            self.history.lock().await.push(ApiMessage {
                role: "user".into(),
                content: ApiContent::Blocks(results),
            });
        }
        Err(AgentError::TooManyRounds)
    }

    async fn call_messages(&self) -> Result<MessagesResp, AgentError> {
        let token = self.auth.access_token().await.map_err(AgentError::Auth)?;
        let history = self.history.lock().await.clone();
        let body = MessagesReq {
            model: DEFAULT_MODEL,
            max_tokens: 8192,
            system: vec![
                SystemBlock { kind: "text", text: CLAUDE_CODE_PREFIX.into() },
                SystemBlock {
                    kind: "text",
                    text: format!(
                        "You are operating as `catbus-agent` inside tab-atelier. \
                         Current working directory: {}. \
                         Plan-mode is {}.",
                        self.session.cwd.display(),
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
    #[error("tool loop ran longer than 32 rounds")]
    TooManyRounds,
}

