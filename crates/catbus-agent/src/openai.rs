// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `OpenAI`-compatible chat-completions backend — any service that
//! speaks the `POST {base}/chat/completions` dialect with Bearer-token
//! auth: `x.ai` (`https://api.x.ai/v1`, Grok models), Infomaniak AI
//! Tools, `OpenAI` itself, a local `llama.cpp`/Ollama server, etc.
//!
//! The agent keeps its history in Anthropic Messages shape (the same
//! blocks the JSONL transcript stores), so this module translates in
//! both directions: history + tool specs → chat-completions request
//! JSON, and the chat-completions response → the `Block` list the
//! tool loop already understands. Nothing outside `call_messages`
//! knows which backend produced a turn.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{ApiContent, ApiMessage, MessagesResp, Usage};
use crate::session::Block;

/// Default model requested from Infomaniak. Must be one of their
/// function-calling-capable models or the tool loop degrades to
/// text-only answers.
pub const INFOMANIAK_DEFAULT_MODEL: &str = "mistral24b";

/// Everything needed to reach one `OpenAI`-compatible service.
pub struct Config {
    /// Full chat-completions endpoint URL.
    pub chat_url: String,
    pub token: String,
    pub model: String,
}

/// Turn a base URL (`https://api.x.ai/v1`) into the full
/// chat-completions endpoint. A URL that already ends in
/// `/chat/completions` passes through untouched, so both spellings
/// work on the CLI.
#[must_use]
pub fn chat_url_from_base(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

/// Infomaniak AI Tools scopes its `OpenAI`-compatible endpoint under a
/// product id: `https://api.infomaniak.com/1/ai/{product_id}/openai`.
#[must_use]
pub fn infomaniak_chat_url(product_id: &str) -> String {
    format!("https://api.infomaniak.com/1/ai/{product_id}/openai/chat/completions")
}

/// Build the chat-completions request body from the agent's
/// Anthropic-shaped state. `system` becomes the leading `system`
/// message; `tool_specs` are the Anthropic-format specs from
/// `tools::tool_specs()`.
#[must_use]
pub fn build_request(model: &str, system: &str, tool_specs: &[Value], history: &[ApiMessage]) -> Value {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(json!({ "role": "system", "content": system }));
    for msg in history {
        convert_message(msg, &mut messages);
    }
    let tools: Vec<Value> = tool_specs.iter().map(tool_to_openai).collect();
    json!({
        "model": model,
        "max_tokens": 8192,
        "messages": messages,
        "tools": tools,
    })
}

/// Anthropic tool spec `{name, description, input_schema}` →
/// `OpenAI` `{type: "function", function: {name, description, parameters}}`.
fn tool_to_openai(spec: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.get("name").cloned().unwrap_or_default(),
            "description": spec.get("description").cloned().unwrap_or_default(),
            "parameters": spec
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        }
    })
}

fn convert_message(msg: &ApiMessage, out: &mut Vec<Value>) {
    match (msg.role.as_str(), &msg.content) {
        ("user", ApiContent::Plain(text)) => out.push(json!({ "role": "user", "content": text })),
        ("user", ApiContent::Blocks(blocks)) => {
            // tool_result blocks become individual `tool` messages and
            // must directly follow the assistant turn that carried the
            // matching tool_calls; any interleaved text trails as a
            // regular user message.
            let mut text = String::new();
            for block in blocks {
                match block {
                    Block::ToolResult {
                        tool_use_id, content, ..
                    } => out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    })),
                    Block::Text { text: t } => append_line(&mut text, t),
                    Block::ToolUse { .. } => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text }));
            }
        }
        ("assistant", ApiContent::Plain(text)) => out.push(json!({ "role": "assistant", "content": text })),
        ("assistant", ApiContent::Blocks(blocks)) => {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in blocks {
                match block {
                    Block::Text { text: t } => append_line(&mut text, t),
                    Block::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        // OpenAI wire format carries arguments as a
                        // JSON *string*, not an object.
                        "function": { "name": name, "arguments": input.to_string() },
                    })),
                    Block::ToolResult { .. } => {}
                }
            }
            if text.is_empty() && tool_calls.is_empty() {
                return;
            }
            let mut m = json!({ "role": "assistant", "content": text });
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(m);
        }
        _ => {}
    }
}

fn append_line(acc: &mut String, part: &str) {
    if !acc.is_empty() {
        acc.push('\n');
    }
    acc.push_str(part);
}

// --- response side ---------------------------------------------------------

#[derive(Deserialize)]
pub struct ChatResp {
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Deserialize)]
struct ToolCall {
    id: String,
    function: FunctionCall,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Fold the first choice of a chat-completions response into the
/// Anthropic-shaped `MessagesResp` the agent loop consumes. Finish
/// reasons are mapped onto Anthropic stop reasons so the loop's
/// `end_turn` check works unchanged.
pub fn into_messages_resp(resp: ChatResp) -> MessagesResp {
    let mut content: Vec<Block> = Vec::new();
    let stop_reason = resp.choices.into_iter().next().and_then(|choice| {
        if let Some(text) = choice.message.content
            && !text.is_empty()
        {
            content.push(Block::Text { text });
        }
        for tc in choice.message.tool_calls {
            // Arguments arrive as a JSON string; a model emitting
            // malformed JSON gets an empty input and the tool's own
            // validation error reported back to it.
            let input: Value = serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
            content.push(Block::ToolUse {
                id: tc.id,
                name: tc.function.name,
                input,
            });
        }
        choice.finish_reason.map(|r| match r.as_str() {
            "stop" => "end_turn".to_string(),
            "tool_calls" | "function_call" => "tool_use".to_string(),
            "length" => "max_tokens".to_string(),
            _ => r,
        })
    });
    MessagesResp {
        content,
        model: resp.model,
        stop_reason,
        usage: Usage {
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
        },
    }
}
