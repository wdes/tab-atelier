// SPDX-License-Identifier: MPL-2.0

//! The `OpenAI` Chat Completions wire.
//!
//! Claude Code speaks the Anthropic Messages API and nothing else, so a provider
//! that answers `OpenAI`'s shape needs both halves rewritten: the request on the
//! way out, and the streamed response on the way back. [`Wire`] is where that is
//! decided; this module is what does it.
//!
//! [`Wire`]: crate::provider::Wire
//!
//! Two properties matter more than fidelity, and both are deliberate:
//!
//! * The translator emits **Anthropic-shaped** usage, so the existing
//!   [`Sniffer`] reads the translated stream without knowing `OpenAI` exists.
//!   Billing, `/me/usage` and the inspection panel therefore need no
//!   OpenAI-aware code at all.
//! * Nothing here can panic. A malformed `arguments` fragment, an empty
//!   `choices`, a missing field — each degrades to a documented fallback, because
//!   the alternative is a dropped request in the middle of someone's turn.
//!
//! [`Sniffer`]: crate::usage::Sniffer

use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::{Value, json};

use crate::usage::Tokens;

/// The most `stop` sequences `OpenAI` accepts. The rest are dropped rather than
/// failing the request over a limit the client cannot see.
const MAX_STOP_SEQUENCES: usize = 4;

/// The endpoint a chat-completions provider is reached at.
///
/// `base` is stored without a trailing slash, but a hand-edited
/// `providers.json` may have one, so both are accepted.
#[must_use]
pub fn chat_url(base: &str) -> String {
    format!("{}/chat/completions", base.trim_end_matches('/'))
}

/// Rewrites an Anthropic Messages request as an `OpenAI` chat-completions one.
///
/// `cache_control` is stripped everywhere: `OpenAI` caches by prefix implicitly,
/// so a breakpoint is not merely unsupported, it is meaningless, and leaving it
/// would be a field the far end rejects.
///
/// `thinking` and `redacted_thinking` blocks are dropped. They are a record of
/// some other model's reasoning; replaying them to `OpenAI` would be wrong even if
/// the wire could carry them.
#[must_use]
pub fn to_chat(anthropic: &Value) -> Value {
    let mut messages = Vec::new();

    if let Some(message) = anthropic.get("system").and_then(system_message) {
        messages.push(message);
    }

    if let Some(turns) = anthropic.get("messages").and_then(Value::as_array) {
        for turn in turns {
            convert_message(turn, &mut messages);
        }
    }

    let mut out = json!({
        "model": anthropic.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
    });

    if let Some(value) = anthropic.get("max_tokens") {
        // `max_completion_tokens`, not the legacy `max_tokens`: the GPT-5.6
        // family is a reasoning series, which rejects the old field outright.
        out["max_completion_tokens"] = value.clone();
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = anthropic.get(field) {
            out[field] = value.clone();
        }
    }

    // The reasoning models do not merely ignore `stop`, they refuse the
    // request: `400 Unsupported parameter: 'stop' is not supported with this
    // model`, and an empty array is refused just the same. Dropping the
    // sequences is the only way to get the call through — which costs the tag
    // a caller may be waiting on (Claude Code's auto-mode classifier stops on
    // one), so the decision is keyed on the model rather than the wire:
    // earlier models honour the field, and compatible servers built on them
    // still get it.
    let model = anthropic.get("model").and_then(Value::as_str).unwrap_or_default();
    if !rejects_stop(model)
        && let Some(stops) = anthropic.get("stop_sequences").and_then(Value::as_array)
    {
        let capped: Vec<&Value> = stops.iter().take(MAX_STOP_SEQUENCES).collect();
        if !capped.is_empty() {
            out["stop"] = json!(capped);
        }
    }

    let mut carries_tools = false;
    if let Some(tools) = anthropic.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().map(convert_tool).collect();
        if !converted.is_empty() {
            out["tools"] = json!(converted);
            carries_tools = true;
        }
    }

    // Reasoning off is what this wire needs in two unrelated cases, and each
    // arrives on a different field.
    //
    // A tool-carrying request. Chat Completions refuses function tools on a
    // reasoning model unless reasoning is switched off; the API says so itself:
    //
    //     Function tools with reasoning_effort are not supported for
    //     gpt-5.6-<tier> in /v1/chat/completions. To use function tools, use
    //     /v1/responses or set reasoning_effort to 'none'.
    //
    // A coding agent sends tools on nearly every turn, so without this the
    // adapter 400s on the requests that matter most. The Responses API is the
    // other road; it is a different body shape and would rewrite both
    // translators, so it is out of scope here. Note this is a real trade:
    // tool-using turns get no hidden reasoning, which is also the cheaper and
    // more predictable thing to bill for.
    //
    // A request that disabled thinking. `thinking` has no place on this wire
    // and is dropped, so the intent has to ride the field this wire does have
    // — otherwise the reasoning model is left on its default and spends the
    // caller's entire token budget thinking before it answers. That is fatal
    // to the auto-mode classifier, which asks for 64 tokens and a verdict and
    // gets an empty string if every one of them goes to reasoning.
    if carries_tools || thinking_is_disabled(anthropic) {
        out["reasoning_effort"] = json!("none");
    }

    if let Some(choice) = anthropic.get("tool_choice").and_then(convert_tool_choice) {
        out["tool_choice"] = choice;
    }

    // The translator needs a usage block on the final chunk, and OpenAI only
    // sends one when asked. Without `stream_options` the accounting would read
    // zero for every streamed call.
    if anthropic.get("stream").and_then(Value::as_bool) == Some(true) {
        out["stream"] = json!(true);
        out["stream_options"] = json!({"include_usage": true});
    }

    out
}

/// Whether the model refuses `stop` on the chat-completions wire.
///
/// The reasoning series does — `gpt-5` and the `o`-prefixed models answer
/// `400 Unsupported parameter: 'stop' is not supported with this model`, and an
/// empty array is refused no differently. Earlier models (`gpt-4o`, `gpt-4.1`,
/// `gpt-3.5`) still honour the field, as do compatible servers built on them,
/// so the decision is the model's and not the wire's.
fn rejects_stop(model: &str) -> bool {
    // Providers prefix their ids (`openai/gpt-5.6-luna`), so compare the last
    // segment only. The `o`-series needs the boundary check, or `o200k` — a
    // tokenizer, not a model — would match.
    let name = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    name.starts_with("gpt-5")
        || ["o1", "o2", "o3", "o4"].iter().any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

/// Whether the caller asked for no hidden reasoning.
///
/// Claude Code spells it `thinking: {"type": "disabled"}`. The key is dropped on
/// the way out — it has no place on this wire — so the intent has to ride the
/// field this wire does have.
fn thinking_is_disabled(anthropic: &Value) -> bool {
    anthropic
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "disabled")
}

/// Flattens an Anthropic `system` field into the single message `OpenAI` expects.
///
/// The field is a string or an array of blocks; only text blocks survive, joined
/// by a newline.
fn system_message(system: &Value) -> Option<Value> {
    let text = if let Some(text) = system.as_str() {
        text.to_owned()
    } else {
        join_text_blocks(system.as_array()?)
    };
    if text.is_empty() {
        return None;
    }
    Some(json!({"role": "system", "content": text}))
}

/// Joins the `text` of every text block, ignoring block types that have none.
fn join_text_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Rewrites one Anthropic turn into the zero or more `OpenAI` messages it becomes.
fn convert_message(turn: &Value, out: &mut Vec<Value>) {
    let role = turn.get("role").and_then(Value::as_str).unwrap_or_default();
    let Some(content) = turn.get("content") else {
        return;
    };

    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            out.push(json!({"role": role, "content": text}));
        }
        return;
    }

    let Some(blocks) = content.as_array() else {
        return;
    };

    if role == "assistant" {
        convert_assistant(blocks, out);
    } else {
        convert_user(blocks, out);
    }
}

/// An assistant turn: prose, tool calls, or both, as one message.
fn convert_assistant(blocks: &[Value], out: &mut Vec<Value>) {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    text.push(part.to_owned());
                }
            }
            Some("tool_use") => calls.push(convert_tool_use(block)),
            // `thinking`, `redacted_thinking` and anything unrecognised: a
            // foreign model's reasoning has no place in this request.
            _ => {}
        }
    }
    // An assistant turn that carried nothing a chat model can read — a lone
    // thinking block, say — is dropped rather than sent as an empty turn,
    // which OpenAI rejects.
    if text.is_empty() && calls.is_empty() {
        return;
    }
    let joined = text.join("\n");
    let mut message = json!({
        "role": "assistant",
        "content": if joined.is_empty() { Value::Null } else { json!(joined) },
    });
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    out.push(message);
}

/// A user turn: tool results first, then whatever prose shared the turn.
///
/// `OpenAI` requires every tool message to immediately follow the assistant turn
/// that requested it, so a tool result behind a line of user prose would make the
/// request malformed. The Anthropic order is the other way round.
fn convert_user(blocks: &[Value], out: &mut Vec<Value>) {
    let mut text = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => out.push(convert_tool_result(block)),
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    text.push(part.to_owned());
                }
            }
            // Images and documents have no chat-completions equivalent here; a
            // user turn that held only one is dropped rather than sent empty.
            _ => {}
        }
    }
    if !text.is_empty() {
        out.push(json!({"role": "user", "content": text.join("\n")}));
    }
}

/// One Anthropic `tool_use` block as an `OpenAI` tool call.
///
/// `arguments` is a JSON *string* on this wire, and the `input` object is
/// already JSON, so it is re-encoded rather than passed through.
fn convert_tool_use(block: &Value) -> Value {
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": block.get("id").cloned().unwrap_or(Value::Null),
        "type": "function",
        "function": {
            "name": block.get("name").cloned().unwrap_or(Value::Null),
            "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned()),
        },
    })
}

/// One Anthropic `tool_result` block as an `OpenAI` `role: "tool"` message.
///
/// An error result is prefixed, since this wire carries no error flag and the
/// model would otherwise read a failure as an answer.
fn convert_tool_result(block: &Value) -> Value {
    let text = block.get("content").map_or_else(String::new, result_text);
    let text = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
        format!("Error: {text}")
    } else {
        text
    };
    json!({
        "role": "tool",
        "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
        "content": text,
    })
}

/// The readable text of a `tool_result` content field, which is a string or an
/// array of blocks.
fn result_text(content: &Value) -> String {
    content.as_str().map_or_else(
        || {
            content
                .as_array()
                .map_or_else(String::new, |blocks| join_text_blocks(blocks))
        },
        str::to_owned,
    )
}

/// One Anthropic tool definition as an `OpenAI` function tool.
fn convert_tool(tool: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.get("name").cloned().unwrap_or(Value::Null),
            "description": tool.get("description").cloned().unwrap_or(Value::Null),
            "parameters": tool.get("input_schema").cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        },
    })
}

/// An Anthropic `tool_choice` as its `OpenAI` equivalent, or `None` to leave the
/// default alone.
fn convert_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => Some(json!({
            "type": "function",
            "function": {"name": choice.get("name").cloned().unwrap_or(Value::Null)},
        })),
        _ => None,
    }
}

/// The first element of a response's `choices`, if it has one.
fn first_choice(payload: &Value) -> Option<&Value> {
    payload.get("choices")?.as_array()?.first()
}

/// Rewrites a complete (non-streamed) `OpenAI` reply as an Anthropic Messages
/// response.
#[must_use]
pub fn from_chat(chat: &Value) -> Value {
    let choice = first_choice(chat);
    let message = choice.and_then(|choice| choice.get("message"));

    let mut content = Vec::new();
    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            content.push(from_tool_call(call));
        }
    }

    json!({
        "id": chat.get("id").cloned().unwrap_or(Value::Null),
        "type": "message",
        "role": "assistant",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": stop_reason(choice.and_then(|choice| choice.get("finish_reason"))),
        "usage": usage_json(&tokens_of(chat.get("usage"))),
    })
}

/// One `OpenAI` tool call as an Anthropic `tool_use` block.
///
/// `arguments` is a JSON string that is not guaranteed to parse — a truncated
/// stream yields a fragment. An unparseable one becomes `{}` rather than an
/// error, matching what the client does with a bad tool input.
fn from_tool_call(call: &Value) -> Value {
    let function = call.get("function");
    let arguments = function
        .and_then(|function| function.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    json!({
        "type": "tool_use",
        "id": call.get("id").cloned().unwrap_or(Value::Null),
        "name": function
            .and_then(|function| function.get("name"))
            .cloned()
            .unwrap_or(Value::Null),
        "input": input,
    })
}

/// `OpenAI`'s `finish_reason` as Anthropic's `stop_reason`.
///
/// Unrecognised reasons become `end_turn` rather than being passed through: a
/// client that cannot parse the value may treat the whole response as bad, and a
/// completed answer labelled oddly is better than a discarded one.
#[must_use]
pub fn stop_reason(finish_reason: Option<&Value>) -> &'static str {
    match finish_reason.and_then(Value::as_str) {
        Some("length") => "max_tokens",
        Some("tool_calls" | "function_call") => "tool_use",
        _ => "end_turn",
    }
}

/// Reads the usage block of an `OpenAI` response into a [`Tokens`].
///
/// The subtraction is the whole subtlety: `OpenAI`'s `prompt_tokens` *includes*
/// the cached ones, while Anthropic's `input_tokens` *excludes* them. Splitting
/// it this way keeps `Tokens::total()` equal to `prompt_tokens`, which is what
/// the existing sniffer and the billing path assume.
#[must_use]
pub fn tokens_of(usage: Option<&Value>) -> Tokens {
    let Some(usage) = usage else {
        return Tokens::default();
    };
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    Tokens {
        input: prompt.saturating_sub(cached),
        output: usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read: cached,
        cache_write: 0,
    }
}

/// A [`Tokens`] as the `usage` object an Anthropic client expects.
#[must_use]
pub fn usage_json(tokens: &Tokens) -> Value {
    json!({
        "input_tokens": tokens.input,
        "output_tokens": tokens.output,
        "cache_read_input_tokens": tokens.cache_read,
        "cache_creation_input_tokens": tokens.cache_write,
    })
}

/// An upstream error as the Anthropic error envelope, keeping the status.
#[must_use]
pub fn to_anthropic_error(status: u16, body: &[u8]) -> Value {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| String::from_utf8_lossy(body).trim().to_owned(), str::to_owned);
    let kind = match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    };
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

/// Translates an `OpenAI` chat-completions SSE stream into an Anthropic one.
///
/// The client is Claude Code, which expects the Anthropic event sequence:
///
/// ```text
/// message_start
///   content_block_start / content_block_delta* / content_block_stop   (per block)
/// message_delta
/// message_stop
/// ```
///
/// `feed` is called with whatever the socket produced, which is not aligned to
/// lines, so it buffers the tail of a partial line and resumes there next time.
#[derive(Debug, Default)]
pub struct Translator {
    /// Bytes since the last newline, kept across `feed` calls.
    pending: Vec<u8>,
    /// Whether `message_start` has been emitted, so it happens exactly once.
    started: bool,
    /// The Anthropic content-block index to assign to the next block.
    next_index: usize,
    /// The block currently open, if any. Anthropic requires it closed before the
    /// next one opens, so this is what forces `content_block_stop`.
    open: Option<Open>,
    /// `OpenAI`'s `tool_calls[].index` to the block index we gave it.
    tools: BTreeMap<u64, usize>,
    /// A tool call being assembled before its block could open.
    assembling: Option<Assembling>,
    /// Usage seen so far. `OpenAI` sends it only on the final chunk.
    tokens: Tokens,
    /// The model name, for `message_start`.
    model: Option<String>,
    /// The last `finish_reason` seen, for the closing `message_delta`.
    stop: Option<&'static str>,
    /// Whether the stream announced its end, so closing happens once.
    closed: bool,
}

/// A content block left open on the Anthropic side.
#[derive(Debug, Clone, Copy)]
enum Open {
    Text(usize),
    Tool(usize),
}

/// A tool call whose block has not opened yet, because the stream has not named
/// it.
///
/// A vendor may send the index and the `arguments` fragments before the `name`
/// arrives, and opening a block with an empty name would produce an unusable
/// tool call — so the fragments wait here until the name shows up.
#[derive(Debug, Default)]
struct Assembling {
    id: Option<String>,
    name: Option<String>,
    /// `arguments` fragments that arrived before the block could open.
    arguments: String,
}

impl Translator {
    /// A translator that has yet to see anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Translates as much of `chunk` as is complete, returning Anthropic SSE.
    ///
    /// Any trailing partial line is held back until the next call, so a `data:`
    /// line split across TCP segments still parses.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(cut) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=cut).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            self.line(&line, &mut out);
        }
        out
    }

    /// Closes whatever the stream left open and emits the terminator.
    ///
    /// Called unconditionally once the upstream body ends, including when it
    /// ended early: a client that gets a truncated but well-formed stream keeps
    /// the text it received, where an unterminated one shows an error instead.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.close_open(&mut out);
        if self.started && !self.closed {
            self.message_delta(&mut out);
            out.push(sse("message_stop", &json!({"type": "message_stop"})));
            self.closed = true;
        }
        out
    }

    /// Handles one complete line of the upstream stream.
    fn line(&mut self, line: &str, out: &mut Vec<Bytes>) {
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            // `event:` lines carry no information the JSON does not, and blank
            // lines are the frame separator.
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(error) = value.get("error") {
            out.push(sse("error", &error_event(error)));
            self.closed = true;
            return;
        }
        self.chunk(&value, out);
    }

    /// Handles one decoded chunk of the upstream stream.
    fn chunk(&mut self, chunk: &Value, out: &mut Vec<Bytes>) {
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.tokens = tokens_of(Some(usage));
        }
        if !self.started {
            self.message_start(chunk, out);
        }

        let Some(choice) = first_choice(chunk) else {
            return;
        };
        if let Some(text) = choice
            .get("delta")
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
            && !text.is_empty()
        {
            self.text_delta(text, out);
        }
        if let Some(calls) = choice
            .get("delta")
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                self.tool_delta(call, out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").filter(|reason| !reason.is_null()) {
            self.stop = Some(stop_reason(Some(reason)));
        }
    }

    /// Opens the message, which Anthropic announces before any content.
    fn message_start(&mut self, chunk: &Value, out: &mut Vec<Bytes>) {
        let message = json!({
            "id": chunk.get("id").cloned().unwrap_or(Value::Null),
            "type": "message",
            "role": "assistant",
            "model": self.model.clone().map_or(Value::Null, Value::String),
            // Anthropic announces input tokens up front; OpenAI only reveals
            // them at the end of the stream, so this is what is known now and
            // the final `message_delta` carries the real figure.
            "usage": usage_json(&self.tokens),
        });
        out.push(sse(
            "message_start",
            &json!({"type": "message_start", "message": message}),
        ));
        self.started = true;
    }

    /// Emits a text delta, opening a text block if one is not already open.
    fn text_delta(&mut self, text: &str, out: &mut Vec<Bytes>) {
        // A tool call still waiting to be named cannot open a block, and text
        // means the tool sequence is over: anything half-assembled is dropped.
        self.assembling = None;
        let index = if let Some(Open::Text(index)) = self.open {
            index
        } else {
            self.close_open(out);
            let index = self.take_index();
            out.push(sse(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""},
                }),
            ));
            self.open = Some(Open::Text(index));
            index
        };
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        ));
    }

    /// Emits one fragment of a tool call.
    ///
    /// The `arguments` fragments are passed through verbatim rather than
    /// reassembled: `partial_json` is defined as an arbitrary fragment the client
    /// concatenates, so forwarding is both lossless and cheaper than buffering,
    /// and it means a call of any size costs no memory here.
    fn tool_delta(&mut self, call: &Value, out: &mut Vec<Bytes>) {
        let tool = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let arguments = call
            .get("function")
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        // A call whose block is already open appends straight through.
        if let Some(block) = self.tools.get(&tool).copied() {
            self.assembling = None;
            push_arguments(block, arguments, out);
            return;
        }

        let assembling = self.assembling.get_or_insert_with(Assembling::default);
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            assembling.id = Some(id.to_owned());
        }
        if let Some(name) = call
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
        {
            assembling.name = Some(name.to_owned());
        }
        assembling.arguments.push_str(arguments);

        // The block cannot open until the name is known, and a vendor may send
        // the arguments first. Whatever was buffered is flushed with the block.
        let Some(name) = assembling.name.clone() else {
            return;
        };
        let id = assembling.id.clone().unwrap_or_default();
        let buffered = std::mem::take(&mut assembling.arguments);
        self.assembling = None;
        let block = self.open_tool(tool, &id, &name, out);
        push_arguments(block, &buffered, out);
    }

    /// Opens a tool-use block, closing whatever was open before it.
    fn open_tool(&mut self, tool: u64, id: &str, name: &str, out: &mut Vec<Bytes>) -> usize {
        self.close_open(out);
        let index = self.take_index();
        self.tools.insert(tool, index);
        out.push(sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            }),
        ));
        self.open = Some(Open::Tool(index));
        index
    }

    /// Closes the open content block, if any.
    fn close_open(&mut self, out: &mut Vec<Bytes>) {
        let Some(Open::Text(index) | Open::Tool(index)) = self.open.take() else {
            return;
        };
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }

    /// Emits the final usage and stop reason.
    fn message_delta(&self, out: &mut Vec<Bytes>) {
        out.push(sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop.unwrap_or("end_turn"),
                    "stop_sequence": Value::Null,
                },
                "usage": usage_json(&self.tokens),
            }),
        ));
    }

    /// Reserves and returns the next content-block index.
    const fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

/// One `input_json_delta`, unless there is nothing to say.
fn push_arguments(index: usize, arguments: &str, out: &mut Vec<Bytes>) {
    if arguments.is_empty() {
        return;
    }
    out.push(sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": arguments},
        }),
    ));
}

/// One Anthropic SSE frame.
fn sse(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_owned())
    ))
}

/// A mid-stream error, as Anthropic's error event.
fn error_event(error: &Value) -> Value {
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("api_error");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("upstream error");
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a stream through a fresh translator and returns the Anthropic SSE.
    fn translated(chunks: &[&str]) -> String {
        let mut translator = Translator::new();
        let mut out = String::new();
        for chunk in chunks {
            for frame in translator.feed(chunk.as_bytes()) {
                out.push_str(&String::from_utf8_lossy(&frame));
            }
        }
        for frame in translator.finish() {
            out.push_str(&String::from_utf8_lossy(&frame));
        }
        out
    }

    /// Every decoded SSE payload in a translated stream, in order.
    fn frames(stream: &str) -> impl Iterator<Item = Value> + '_ {
        stream
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
    }

    /// The text deltas of a translated stream, concatenated.
    fn delta_text(stream: &str) -> String {
        frames(stream)
            .filter_map(|frame| frame.get("delta")?.get("text")?.as_str().map(str::to_owned))
            .collect()
    }

    /// The `input_json_delta` fragments of a translated stream, concatenated.
    fn partial_json(stream: &str) -> String {
        frames(stream)
            .filter_map(|frame| frame.get("delta")?.get("partial_json")?.as_str().map(str::to_owned))
            .collect()
    }

    /// The content-block events of a translated stream, as (`type`, `index`).
    ///
    /// Count on this rather than on `stream.matches(..)`: every event name
    /// appears twice in the raw text — once on the `event:` line and once as
    /// the payload's `type` — so substring counting silently doubles.
    fn block_events(stream: &str) -> Vec<(String, u64)> {
        frames(stream)
            .filter_map(|frame| {
                let kind = frame.get("type")?.as_str()?;
                if !kind.starts_with("content_block_") {
                    return None;
                }
                Some((kind.to_owned(), frame.get("index")?.as_u64()?))
            })
            .collect()
    }

    /// The `stop_reason` a translated stream finally reported.
    fn stop_reason_of(stream: &str) -> Value {
        frames(stream)
            .find_map(|frame| frame.get("delta")?.get("stop_reason").cloned())
            .expect("a stop reason")
    }

    /// One upstream SSE frame carrying `chunk`.
    fn upstream(chunk: &Value) -> String {
        format!("data: {chunk}\n\n")
    }

    #[test]
    fn the_endpoint_is_appended_once_however_the_base_was_written() {
        assert_eq!(
            chat_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn a_system_string_and_a_system_block_array_both_become_one_message() {
        let from_string = to_chat(&json!({
            "model": "m",
            "system": "be brief",
            "messages": [],
        }));
        assert_eq!(from_string["messages"][0]["role"], "system");
        assert_eq!(from_string["messages"][0]["content"], "be brief");

        let from_blocks = to_chat(&json!({
            "model": "m",
            "system": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "messages": [],
        }));
        assert_eq!(from_blocks["messages"][0]["content"], "one\ntwo");
    }

    #[test]
    fn tool_results_are_hoisted_ahead_of_the_user_text_that_shared_their_turn() {
        // OpenAI requires a tool message to directly follow the assistant turn
        // that asked for it, so prose in the same Anthropic turn must come after.
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "42"},
                    {"type": "text", "text": "and now?"},
                ],
            }],
        }));
        assert_eq!(chat["messages"][0]["role"], "tool");
        assert_eq!(chat["messages"][0]["tool_call_id"], "t1");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][1]["content"], "and now?");
    }

    #[test]
    fn an_error_tool_result_is_labelled_because_this_wire_has_no_error_flag() {
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": "boom",
                    "is_error": true,
                }],
            }],
        }));
        assert_eq!(chat["messages"][0]["content"], "Error: boom");
    }

    #[test]
    fn a_tool_use_becomes_a_call_whose_arguments_are_a_json_string() {
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/x"}}],
            }],
        }));
        let call = &chat["messages"][0]["tool_calls"][0];
        assert_eq!(call["id"], "t1");
        assert_eq!(call["function"]["name"], "Read");
        // A string, not an object: this is what the wire requires.
        assert_eq!(call["function"]["arguments"], "{\"path\":\"/x\"}");
    }

    #[test]
    fn thinking_is_dropped_and_a_turn_that_held_only_thinking_is_dropped_whole() {
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm"}]},
                {"role": "user", "content": "hello"},
            ],
        }));
        // The empty assistant turn is gone, so the user turn is first.
        assert_eq!(chat["messages"].as_array().expect("array").len(), 1);
        assert_eq!(chat["messages"][0]["role"], "user");
    }

    #[test]
    fn cache_control_is_stripped_from_tools_and_messages() {
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}],
            }],
            "tools": [{
                "name": "Read",
                "description": "read",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"},
            }],
        }));
        let rendered = serde_json::to_string(&chat).expect("serialize");
        assert!(!rendered.contains("cache_control"));
    }

    #[test]
    fn tool_choice_maps_onto_the_openai_vocabulary() {
        let choice = |choice: Value| {
            to_chat(&json!({"model": "m", "messages": [], "tool_choice": choice}))["tool_choice"].clone()
        };
        assert_eq!(choice(json!({"type": "auto"})), json!("auto"));
        assert_eq!(choice(json!({"type": "any"})), json!("required"));
        assert_eq!(choice(json!({"type": "none"})), json!("none"));
        assert_eq!(
            choice(json!({"type": "tool", "name": "Read"}))["function"]["name"],
            "Read"
        );
    }

    #[test]
    fn stop_sequences_are_capped_at_the_four_the_wire_allows() {
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [],
            "stop_sequences": ["a", "b", "c", "d", "e"],
        }));
        assert_eq!(chat["stop"].as_array().expect("array").len(), 4);
    }

    #[test]
    fn a_reasoning_model_is_never_sent_stop_because_it_would_refuse_the_call() {
        // Not "ignores the field" — refuses the request, `400 Unsupported
        // parameter: 'stop' is not supported with this model`, empty array
        // included. The auto-mode classifier always carries `stop_sequences`,
        // so this is what made every classifier call 400 on a gpt-5 alias.
        let chat = to_chat(&json!({
            "model": "gpt-5.6-luna",
            "messages": [],
            "stop_sequences": ["</block>"],
        }));
        assert!(chat.get("stop").is_none());
    }

    #[test]
    fn an_earlier_model_still_gets_its_stop_sequences() {
        // The refusal is a property of the reasoning models, not of the wire:
        // dropping the field for everything would silently break streaming
        // servers that honour it.
        let chat = to_chat(&json!({
            "model": "gpt-4o",
            "messages": [],
            "stop_sequences": ["</block>"],
        }));
        assert_eq!(chat["stop"][0], "</block>");
    }

    #[test]
    fn the_stop_refusal_survives_a_provider_prefix() {
        assert!(rejects_stop("openai/gpt-5.6-luna"));
        assert!(rejects_stop("o3-mini"));
        // A tokenizer name, not a model: the `o` rule needs its boundary.
        assert!(!rejects_stop("o200k"));
        assert!(!rejects_stop("gpt-4.1"));
    }

    #[test]
    fn a_disabled_thinking_turns_reasoning_off_even_with_no_tools() {
        // `thinking` has no home on this wire and is dropped, so the intent
        // has to move to `reasoning_effort`. Without it the classifier's 64
        // tokens are all spent reasoning and the answer comes back empty.
        let chat = to_chat(&json!({
            "model": "gpt-5.6-luna",
            "messages": [],
            "thinking": {"type": "disabled"},
        }));
        assert_eq!(chat["reasoning_effort"], "none");
    }

    #[test]
    fn an_untouched_thinking_block_does_not_force_reasoning_off() {
        assert!(!thinking_is_disabled(
            &json!({"type": "enabled", "budget_tokens": 4000})
        ));
        assert!(!thinking_is_disabled(&json!({})));
    }

    #[test]
    fn a_tool_carrying_request_forces_reasoning_off_because_the_wire_demands_it() {
        // The API rejects the pairing outright: "Function tools with
        // reasoning_effort are not supported ... To use function tools, use
        // reasoning_effort 'none'." Claude Code sends tools on nearly every
        // turn, so this is the common path, not an edge case.
        let chat = to_chat(&json!({
            "model": "m",
            "messages": [],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        }));
        assert_eq!(chat["reasoning_effort"], "none");
        assert_eq!(chat["tools"][0]["type"], "function");
    }

    #[test]
    fn without_tools_the_reasoning_level_is_left_to_the_model() {
        let chat = to_chat(&json!({"model": "m", "messages": []}));
        assert!(chat.get("reasoning_effort").is_none());
    }

    #[test]
    fn a_reasoning_pin_strips_the_tools_so_the_wire_keeps_its_reasoning() {
        // The two halves of the selector's "reasoning, no tools" entry: the
        // account's policy is `mode: none`, so the tools are gone before the
        // body ever reaches this translator — and with none left there is
        // nothing to force reasoning off for.
        let mut body = json!({
            "model": "m",
            "messages": [],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "Bash", "input_schema": {"type": "object"}},
            ],
        });
        let policy = crate::tools::Policy {
            mode: crate::tools::Mode::None,
            ..crate::tools::Policy::default()
        };
        crate::tools::apply(&mut body, &policy, true);

        let chat = to_chat(&body);
        assert!(chat.get("tools").is_none(), "no tools survive: {chat}");
        // With the tools gone there is nothing to force reasoning off for,
        // so the wire keeps whatever reasoning the model defaults to.
        assert!(chat.get("reasoning_effort").is_none());
    }

    #[test]
    fn max_tokens_becomes_the_field_the_reasoning_series_accepts() {
        let chat = to_chat(&json!({"model": "m", "messages": [], "max_tokens": 4096}));
        assert_eq!(chat["max_completion_tokens"], 4096);
        assert!(chat.get("max_tokens").is_none());
    }

    #[test]
    fn streaming_asks_for_the_usage_block_it_would_otherwise_never_see() {
        let chat = to_chat(&json!({"model": "m", "messages": [], "stream": true}));
        assert_eq!(chat["stream_options"]["include_usage"], true);
    }

    #[test]
    fn cached_prompt_tokens_are_subtracted_so_the_total_still_matches() {
        // OpenAI counts cached tokens inside prompt_tokens; Anthropic does not.
        let tokens = tokens_of(Some(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 900},
        })));
        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.cache_read, 900);
        assert_eq!(tokens.output, 50);
        assert_eq!(tokens.total(), 1050);
    }

    #[test]
    fn an_unknown_finish_reason_still_reports_a_finished_turn() {
        assert_eq!(stop_reason(Some(&json!("content_filter"))), "end_turn");
        assert_eq!(stop_reason(Some(&json!("length"))), "max_tokens");
        assert_eq!(stop_reason(Some(&json!("tool_calls"))), "tool_use");
        assert_eq!(stop_reason(None), "end_turn");
    }

    #[test]
    fn an_upstream_error_becomes_the_anthropic_envelope_with_its_status_kept() {
        let error = to_anthropic_error(429, br#"{"error":{"message":"slow down"}}"#);
        assert_eq!(error["error"]["type"], "rate_limit_error");
        assert_eq!(error["error"]["message"], "slow down");
        // A non-JSON body still yields a message rather than an empty one.
        let plain = to_anthropic_error(500, b"upstream exploded");
        assert_eq!(plain["error"]["message"], "upstream exploded");
        assert_eq!(plain["error"]["type"], "api_error");
    }

    #[test]
    fn a_stream_translates_into_the_anthropic_event_sequence() {
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "model": "gpt-5.6-luna",
                "choices": [{"delta": {"content": "Hi"}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"content": " there"}, "finish_reason": "stop"}],
            })),
            &upstream(&json!({
                "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 2},
            })),
            "data: [DONE]\n\n",
        ]);
        assert_eq!(delta_text(&stream), "Hi there");
        for event in [
            "message_start",
            "content_block_start",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(stream.contains(event), "missing {event}");
        }
        // Ordering is what makes it a valid stream, not just presence.
        let order = |needle: &str| stream.find(needle).unwrap_or(usize::MAX);
        assert!(order("message_start") < order("content_block_start"));
        assert!(order("content_block_stop") < order("message_delta"));
        assert!(order("message_delta") < order("message_stop"));
        assert_eq!(stop_reason_of(&stream), json!("end_turn"));
    }

    #[test]
    fn a_line_split_across_two_reads_is_still_one_event() {
        // The socket does not respect line boundaries, so this must not lose the
        // half of the line that arrived first.
        let stream = translated(&[
            "data: {\"id\":\"c1\",\"cho",
            "ices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        assert_eq!(delta_text(&stream), "ok");
    }

    #[test]
    fn a_stream_fed_one_byte_at_a_time_translates_identically() {
        let whole = format!(
            "{}{}",
            upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}],
            })),
            upstream(&json!({
                "choices": [],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3},
            })),
        );
        let mut translator = Translator::new();
        let mut out = String::new();
        for byte in whole.as_bytes() {
            for frame in translator.feed(&[*byte]) {
                out.push_str(&String::from_utf8_lossy(&frame));
            }
        }
        for frame in translator.finish() {
            out.push_str(&String::from_utf8_lossy(&frame));
        }
        assert_eq!(delta_text(&out), "ok");
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn a_tool_call_split_into_fragments_reassembles_into_the_same_arguments() {
        // The client concatenates `partial_json`, so the fragments must survive
        // byte for byte and in order — asserted by reassembling them.
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "t1",
                    "function": {"name": "Read", "arguments": "{\"pa"},
                }]}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "th\":\"/x"},
                }]}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\"}"},
                }]}, "finish_reason": "tool_calls"}],
            })),
            "data: [DONE]\n\n",
        ]);
        assert_eq!(partial_json(&stream), "{\"path\":\"/x\"}");
        // One block, opened once, however many fragments carried it.
        assert_eq!(stream.matches("\"type\":\"tool_use\"").count(), 1);
        assert!(stream.contains("\"name\":\"Read\""));
        assert_eq!(stop_reason_of(&stream), json!("tool_use"));
        let events = block_events(&stream);
        assert_eq!(
            events.iter().filter(|(kind, _)| kind == "content_block_start").count(),
            1,
            "a lagging name must open exactly one block: {events:?}"
        );
    }

    #[test]
    fn a_tool_call_whose_name_lags_its_index_waits_before_opening_a_block() {
        // Some vendors send the index and arguments before the name; opening a
        // block with an empty name would produce an unusable tool call, and the
        // fragments that arrived first must not be lost.
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "{\"path\":\"/x\"}"},
                }]}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "t1",
                    "function": {"name": "Read"},
                }]}}],
            })),
            "data: [DONE]\n\n",
        ]);
        let starts = block_events(&stream)
            .iter()
            .filter(|(kind, _)| kind == "content_block_start")
            .count();
        assert_eq!(starts, 1, "the buffered fragment must wait for the name");
        assert!(stream.contains("\"name\":\"Read\""));
        assert_eq!(partial_json(&stream), "{\"path\":\"/x\"}");
    }

    #[test]
    fn two_tool_calls_become_two_blocks_with_their_own_arguments() {
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "t1", "function": {"name": "Read", "arguments": "{}"}},
                ]}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 1, "id": "t2", "function": {"name": "Glob", "arguments": "{}"}},
                ]}}],
            })),
            "data: [DONE]\n\n",
        ]);
        assert_eq!(stream.matches("\"type\":\"tool_use\"").count(), 2);
        assert!(stream.contains("\"name\":\"Read\""));
        assert!(stream.contains("\"name\":\"Glob\""));
        // Open, close, open, close — never two blocks open at once, and each
        // stop names the index its start did.
        let events = block_events(&stream);
        let opened: Vec<u64> = events
            .iter()
            .filter(|(kind, _)| kind == "content_block_start")
            .map(|(_, index)| *index)
            .collect();
        let closed: Vec<u64> = events
            .iter()
            .filter(|(kind, _)| kind == "content_block_stop")
            .map(|(_, index)| *index)
            .collect();
        assert_eq!(opened, closed);
        assert_eq!(opened.len(), 2);
    }

    #[test]
    fn text_then_a_tool_then_text_gets_three_distinct_block_indexes() {
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"content": "one"}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "t1",
                    "function": {"name": "Read", "arguments": "{}"},
                }]}}],
            })),
            &upstream(&json!({
                "choices": [{"delta": {"content": "two"}, "finish_reason": "stop"}],
            })),
            "data: [DONE]\n\n",
        ]);
        let kinds: Vec<Value> = frames(&stream)
            .filter_map(|frame| frame.get("content_block")?.get("type").cloned())
            .collect();
        assert_eq!(kinds, vec![json!("text"), json!("tool_use"), json!("text")]);
        // The block each event targets matches the block that was opened for it.
        // Three blocks, each start/delta/stop triple carrying the same index.
        let indexes: Vec<u64> = block_events(&stream).into_iter().map(|(_, index)| index).collect();
        assert_eq!(indexes, vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
        assert_eq!(delta_text(&stream), "onetwo");
    }

    #[test]
    fn a_stream_cut_off_before_its_sentinel_still_closes_everything() {
        // The client should keep the partial answer rather than report a broken
        // stream, so the closers are emitted even though [DONE] never arrived.
        let stream = translated(&[&upstream(&json!({
            "id": "c1",
            "choices": [{"delta": {"content": "half"}}],
        }))]);
        assert_eq!(delta_text(&stream), "half");
        assert!(stream.contains("content_block_stop"));
        assert!(stream.contains("message_delta"));
        assert!(stream.contains("message_stop"));
    }

    #[test]
    fn closing_twice_emits_nothing_the_second_time() {
        let mut translator = Translator::new();
        translator.feed(
            upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"content": "x"}}],
            }))
            .as_bytes(),
        );
        let first = translator.finish();
        let second = translator.finish();
        assert!(!first.is_empty());
        assert!(second.is_empty());
    }

    #[test]
    fn a_mid_stream_error_becomes_an_error_event_and_stops_translation() {
        let stream = translated(&[
            &upstream(&json!({
                "id": "c1",
                "choices": [{"delta": {"content": "x"}}],
            })),
            &upstream(&json!({
                "error": {"type": "server_error", "message": "upstream died"},
            })),
        ]);
        assert!(stream.contains("event: error"));
        assert!(stream.contains("upstream died"));
        // After an error the stream is over: no second message_start.
        assert_eq!(stream.matches("event: message_start").count(), 1);
    }

    #[test]
    fn a_non_streamed_reply_becomes_a_messages_response() {
        let messages = from_chat(&json!({
            "id": "c1",
            "model": "gpt-5.6-luna",
            "choices": [{
                "message": {
                    "content": "hello",
                    "tool_calls": [{
                        "id": "t1",
                        "function": {"name": "Read", "arguments": "{\"path\":\"/x\"}"},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7},
        }));
        assert_eq!(messages["type"], "message");
        assert_eq!(messages["stop_reason"], "tool_use");
        assert_eq!(messages["content"][0]["text"], "hello");
        assert_eq!(messages["content"][1]["type"], "tool_use");
        assert_eq!(messages["content"][1]["input"]["path"], "/x");
        assert_eq!(messages["usage"]["input_tokens"], 5);
    }

    #[test]
    fn unparseable_tool_arguments_become_an_empty_input_rather_than_an_error() {
        // A truncated stream yields a fragment that is not JSON; the client
        // reports the bad input itself, which is better than losing the turn.
        let messages = from_chat(&json!({
            "id": "c1",
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "t1",
                        "function": {"name": "Read", "arguments": "{\"pa"},
                    }],
                },
            }],
        }));
        assert_eq!(messages["content"][0]["input"], json!({}));
    }
}
