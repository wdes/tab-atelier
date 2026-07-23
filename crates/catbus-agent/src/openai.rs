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

#[cfg(test)]
mod tests {
    use super::*;

    fn user_plain(text: &str) -> ApiMessage {
        ApiMessage {
            role: "user".into(),
            content: ApiContent::Plain(text.into()),
        }
    }

    fn blocks(role: &str, blocks: Vec<Block>) -> ApiMessage {
        ApiMessage {
            role: role.into(),
            content: ApiContent::Blocks(blocks),
        }
    }

    #[test]
    fn chat_url_from_base_appends_endpoint() {
        assert_eq!(
            chat_url_from_base("https://api.x.ai/v1"),
            "https://api.x.ai/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_from_base_strips_trailing_slash() {
        assert_eq!(
            chat_url_from_base("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_from_base_accepts_full_endpoint() {
        assert_eq!(
            chat_url_from_base("https://api.x.ai/v1/chat/completions"),
            "https://api.x.ai/v1/chat/completions"
        );
    }

    #[test]
    fn infomaniak_chat_url_is_product_scoped() {
        assert_eq!(
            infomaniak_chat_url("12345"),
            "https://api.infomaniak.com/1/ai/12345/openai/chat/completions"
        );
    }

    #[test]
    fn build_request_converts_history_and_tools() {
        let history = vec![
            user_plain("read hello.txt please"),
            blocks(
                "assistant",
                vec![
                    Block::Text { text: "on it".into() },
                    Block::ToolUse {
                        id: "call_1".into(),
                        name: "Read".into(),
                        input: json!({"path": "hello.txt"}),
                    },
                ],
            ),
            blocks(
                "user",
                vec![Block::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "mock says hi".into(),
                    is_error: false,
                }],
            ),
        ];
        let specs = crate::tools::tool_specs();
        let req = build_request("test-model", "be helpful", &specs, &history);

        assert_eq!(req["model"], "test-model");
        assert_eq!(req["max_tokens"], 8192);

        let messages = req["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be helpful");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "read hello.txt please");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "on it");
        let call = &messages[2]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "Read");
        // Arguments cross the wire as a JSON *string*, not an object.
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "hello.txt");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "mock says hi");

        // Every agent tool must survive the spec conversion.
        let tools = req["tools"].as_array().unwrap();
        assert_eq!(tools.len(), specs.len());
        for (converted, spec) in tools.iter().zip(&specs) {
            assert_eq!(converted["type"], "function");
            assert_eq!(converted["function"]["name"], spec["name"]);
            assert_eq!(converted["function"]["parameters"], spec["input_schema"]);
        }
    }

    #[test]
    fn build_request_splits_tool_results_from_trailing_text() {
        let history = vec![blocks(
            "user",
            vec![
                Block::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "output".into(),
                    is_error: true,
                },
                Block::Text {
                    text: "carry on".into(),
                },
                // A tool_use inside a *user* turn is malformed history
                // (only assistants call tools) — it must be dropped,
                // not forwarded.
                Block::ToolUse {
                    id: "bogus".into(),
                    name: "Bash".into(),
                    input: json!({}),
                },
            ],
        )];
        let req = build_request("m", "s", &[], &history);
        let messages = req["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "carry on");
    }

    #[test]
    fn build_request_passes_plain_assistant_and_skips_unknown_roles() {
        let history = vec![
            ApiMessage {
                role: "assistant".into(),
                content: ApiContent::Plain("prior answer".into()),
            },
            ApiMessage {
                role: "system".into(),
                content: ApiContent::Plain("not a turn".into()),
            },
        ];
        let req = build_request("m", "s", &[], &history);
        let messages = req["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "prior answer");
    }

    #[test]
    fn tool_spec_without_schema_gets_empty_parameters() {
        let specs = [json!({ "name": "Bare" })];
        let req = build_request("m", "s", &specs, &[]);
        let tool = &req["tools"][0]["function"];
        assert_eq!(tool["name"], "Bare");
        assert_eq!(tool["parameters"], json!({ "type": "object", "properties": {} }));
    }

    #[test]
    fn build_request_drops_empty_assistant_turns() {
        let history = vec![blocks("assistant", vec![])];
        let req = build_request("m", "s", &[], &history);
        // Only the system message survives.
        assert_eq!(req["messages"].as_array().unwrap().len(), 1);
    }

    /// Mock of a plain text answer, `x.ai` / `OpenAI` shape.
    const TEXT_RESPONSE: &str = r#"{
        "id": "cmpl-1",
        "object": "chat.completion",
        "model": "grok-4-0709",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello from grok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 }
    }"#;

    /// Mock of a function-call answer with an empty content string,
    /// as Grok and Mistral-family models emit while calling tools.
    const TOOL_CALL_RESPONSE: &str = r#"{
        "id": "cmpl-2",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_9",
                    "type": "function",
                    "function": { "name": "Bash", "arguments": "{\"command\":\"ls\"}" }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 30, "completion_tokens": 9 }
    }"#;

    #[test]
    fn text_response_maps_to_end_turn() {
        let resp: ChatResp = serde_json::from_str(TEXT_RESPONSE).unwrap();
        let out = into_messages_resp(resp);
        assert_eq!(out.model, "grok-4-0709");
        assert_eq!(out.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(out.usage.input_tokens, 12);
        assert_eq!(out.usage.output_tokens, 4);
        assert_eq!(out.content.len(), 1);
        assert!(matches!(&out.content[0], Block::Text { text } if text == "hello from grok"));
    }

    #[test]
    fn tool_call_response_maps_to_tool_use() {
        let resp: ChatResp = serde_json::from_str(TOOL_CALL_RESPONSE).unwrap();
        let out = into_messages_resp(resp);
        assert_eq!(out.stop_reason.as_deref(), Some("tool_use"));
        // The empty content string must not become an empty Text block.
        assert_eq!(out.content.len(), 1);
        match &out.content[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn malformed_tool_arguments_become_empty_input() {
        let raw = r#"{
            "model": "m",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "Read", "arguments": "{not json" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatResp = serde_json::from_str(raw).unwrap();
        let out = into_messages_resp(resp);
        assert!(matches!(&out.content[0], Block::ToolUse { input, .. } if *input == json!({})));
    }

    #[test]
    fn length_and_unknown_finish_reasons() {
        for (wire, mapped) in [("length", "max_tokens"), ("content_filter", "content_filter")] {
            let raw = format!(
                r#"{{"model":"m","choices":[{{"message":{{"role":"assistant","content":"x"}},"finish_reason":"{wire}"}}]}}"#
            );
            let resp: ChatResp = serde_json::from_str(&raw).unwrap();
            assert_eq!(into_messages_resp(resp).stop_reason.as_deref(), Some(mapped));
        }
    }

    #[test]
    fn empty_choices_yield_empty_resp() {
        let resp: ChatResp = serde_json::from_str(r#"{"model":"m","choices":[]}"#).unwrap();
        let out = into_messages_resp(resp);
        assert!(out.content.is_empty());
        assert!(out.stop_reason.is_none());
        assert_eq!(out.usage.input_tokens, 0);
    }
}
