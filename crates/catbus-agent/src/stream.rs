// SPDX-License-Identifier: MPL-2.0

//! Reassembling a streamed Messages reply.
//!
//! A `stream: true` request is answered with server-sent events, and a client
//! that wants to show the model's reasoning while it works has to put the reply
//! back together as it arrives. This module is that putting-together: it takes
//! the bytes as they come and hands back the same [`MessagesResp`] the
//! unstreamed call used to return, so nothing downstream of the wire has to
//! know which of the two shapes it received.
//!
//! The vocabulary is Anthropic's. `message_start` carries the model and the
//! input token count; `content_block_start` opens a block, `_delta` extends it
//! and `_stop` closes it; `message_delta` carries the stop reason and the
//! output tokens; `message_stop` ends the reply. A block is text, thinking, or a
//! tool call. `ping` frames are ignored, and so is any event type this build has
//! never heard of — the vocabulary is the provider's and it grows, which is not
//! a reason to fail a turn.
//!
//! The framing follows the proxy's own SSE reader (`tab-atelier-proxy`'s
//! `usage::Sniffer`): split on newlines, keep only `data:` lines, and dispatch
//! on the `type` inside the payload rather than on the `event:` line. The
//! `event:` name and the payload's own type say the same thing, and trusting
//! the payload means one fewer thing to keep in step.

use serde_json::{Map, Value};

use crate::agent::{MessagesResp, Usage};
use crate::session::Block;

/// Builds one [`MessagesResp`] out of a streamed reply's bytes.
///
/// Feed it every chunk as it arrives, then call [`finish`](Self::finish) once
/// the body ends. It is also the only place the partial reasoning is available
/// while the reply is still coming, which is what the live view reads.
#[derive(Debug, Default)]
pub struct Assembler {
    /// Bytes of the line that has not ended yet.
    ///
    /// A chunk boundary falls wherever the network put it — mid-line, mid-UTF-8
    /// character, even mid-escape — so a line is only known to be whole once its
    /// newline has arrived. Holding the remainder is what makes that safe; the
    /// alternative, decoding each chunk on its own, corrupts any multi-byte
    /// character unlucky enough to straddle two.
    partial: Vec<u8>,
    model: String,
    usage: Usage,
    stop_reason: Option<String>,
    /// Blocks by the `index` the deltas address.
    ///
    /// Sparse on purpose: an index is only known once its block starts, and the
    /// provider is free to number them as it likes.
    blocks: Vec<Option<Partial>>,
    /// The reasoning so far, which the live view mirrors. Kept beside the block
    /// it belongs to rather than derived from it, so reading it costs nothing
    /// and cannot disturb assembly.
    reasoning: String,
    /// Bytes of reply content received so far: text, reasoning and tool arguments.
    ///
    /// The only output figure that exists while a reply is still coming. The provider
    /// reports `usage.output_tokens` once, in the closing `message_delta` — and the count is
    /// cumulative over the whole reply, so a reader who waits for it has nothing to show
    /// during the wait. This is a count of what has *arrived*, which is what a live view can
    /// honestly report; [`Self::started`] says whether anything has arrived at all, since a
    /// reply that has not begun and one that has produced nothing yet are otherwise both zero.
    output_bytes: u64,
    /// Whether `message_start` has been seen.
    started: bool,
    /// Whether `message_stop` has been seen.
    done: bool,
}

/// A content block that is still being built.
#[derive(Debug)]
enum Partial {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Tool {
        id: String,
        name: String,
        /// The arguments as the fragments arrive, concatenated.
        json: String,
        /// What `content_block_start` said the arguments were.
        ///
        /// Anthropic sends `{}` here and streams the real arguments as
        /// `input_json_delta`s, but a provider that hands over the whole object
        /// up front and sends no deltas is equally valid — and a tool that takes
        /// no arguments sends `{}` with no delta at all. Keeping the opening
        /// value is what lets all three end up as the same arguments.
        opened: Option<Value>,
    },
}

impl Assembler {
    /// A new, empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The reasoning received so far.
    #[must_use]
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// The counts the provider has reported so far, if it has reported any.
    ///
    /// Available as soon as `message_start` lands, which is at the head of the reply: the
    /// input side is known then and never changes, and the output side is a running total
    /// that only becomes final at the close. `None` until the reply begins, so a reader can
    /// tell "no counts yet" from "counts of zero".
    #[must_use]
    pub fn usage(&self) -> Option<&Usage> {
        self.started.then_some(&self.usage)
    }

    /// The model the reply named, if it named one.
    ///
    /// Named by the reply rather than taken from the request, because the relay is free to route a
    /// request to a different model than the session last used — and a figure priced at the wrong
    /// model's rates is worse than no figure. The cost ledger only learns the name from a
    /// *finished* reply, so this is the only source the live row has while the reply is coming.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        (!self.model.is_empty()).then_some(self.model.as_str())
    }

    /// Bytes of reply content received so far. See the field for what it counts.
    #[must_use]
    pub const fn output_bytes(&self) -> u64 {
        self.output_bytes
    }

    /// Whether the reply has begun.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// Whether the reply has said it is finished.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Take the bytes just received from the wire.
    ///
    /// # Errors
    /// Returns the provider's own message when the stream carries an `error`
    /// event, and a description when a `data:` payload is not the JSON it
    /// claims to be. Both are returned rather than logged because the caller is
    /// mid-turn and has to decide what to do about a reply it cannot trust.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.partial.extend_from_slice(chunk);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            self.line(&line)?;
        }
        Ok(())
    }

    /// One complete line, newline included.
    fn line(&mut self, line: &[u8]) -> Result<(), String> {
        // A server is within its rights to use CRLF, and the `\r` would end up
        // inside the JSON.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // The stream is UTF-8 and the newline that delimited this line was
        // ASCII, so an invalid line means the stream itself is damaged. There is
        // nothing to recover, and nothing worth failing a turn over either: a
        // line that is not valid text cannot be a frame we needed.
        let Ok(line) = std::str::from_utf8(line) else {
            return Ok(());
        };
        // `event:`, `id:`, a comment, or the blank line between events. Only the
        // payload matters, and it carries its own type.
        let Some(payload) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let payload = payload.trim();
        // The sentinel some providers close with, and the keep-alive some send
        // between events.
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(payload).map_err(|e| format!("a streamed event was not JSON: {e}"))?;
        self.event(&event)
    }

    /// One decoded event.
    fn event(&mut self, event: &Value) -> Result<(), String> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.message_start(event),
            Some("content_block_start") => self.block_start(event),
            Some("content_block_delta") => self.block_delta(event),
            Some("content_block_stop") => self.block_stop(event),
            Some("message_delta") => self.message_delta(event),
            Some("message_stop") => self.done = true,
            Some("error") => {
                return Err(event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the provider reported an error mid-stream")
                    .to_owned());
            }
            // `ping` — the keep-alive — lands here too, with any frame this build
            // does not know. Ignoring both is the same decision and deserves one
            // arm: a new event type must not end every turn, and neither must a
            // heartbeat.
            _ => {}
        }
        Ok(())
    }

    fn message_start(&mut self, event: &Value) {
        let Some(message) = event.get("message") else {
            return;
        };
        // Set before the fields below, and from the event rather than from their presence: a
        // `message_start` carrying no usage is still a reply that has begun, and the live view
        // reads this to tell a request still waiting on the provider from one being answered.
        self.started = true;
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            model.clone_into(&mut self.model);
        }
        if let Some(usage) = message.get("usage") {
            self.usage = usage_of(usage);
        }
    }

    fn block_start(&mut self, event: &Value) {
        let block = event.get("content_block");
        let kind = block.and_then(|b| b.get("type")).and_then(Value::as_str);
        let text = |field: &str| {
            block
                .and_then(|b| b.get(field))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let partial = match kind {
            Some("text") => Partial::Text(text("text")),
            Some("thinking") => Partial::Thinking {
                text: text("thinking"),
                signature: block
                    .and_then(|b| b.get("signature"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            Some("tool_use") => Partial::Tool {
                id: text("id"),
                name: text("name"),
                json: String::new(),
                opened: block.and_then(|b| b.get("input")).cloned(),
            },
            // A block kind this build cannot represent. Skipping it loses that
            // block, which is worse than it sounds — the history has to be what
            // the model sent — so it is named rather than passed over in
            // silence.
            _ => {
                log::warn!(
                    "a streamed reply opened a content block this build does not know: {}",
                    block.map_or_else(|| "<none>".to_owned(), Value::to_string)
                );
                return;
            }
        };
        self.put(index_of(event), partial);
    }

    fn block_delta(&mut self, event: &Value) {
        let Some(delta) = event.get("delta") else {
            return;
        };
        let field = |name: &str| delta.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
        let index = index_of(event);
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let piece = field("text");
                // Counted as it arrives, which is the only reason the live view can say what a
                // reply has produced before the provider says it. Counted even for a delta whose
                // block is missing — a reply is producing content whatever this build made of the
                // frame, and a figure that silently stopped counting would be the kind of number
                // this display exists to avoid.
                self.output_bytes = self.output_bytes.saturating_add(piece.len() as u64);
                if let Some(Partial::Text(text)) = self.at(index) {
                    text.push_str(&piece);
                }
            }
            Some("thinking_delta") => {
                let piece = field("thinking");
                self.output_bytes = self.output_bytes.saturating_add(piece.len() as u64);
                if let Some(Partial::Thinking { text, .. }) = self.at(index) {
                    text.push_str(&piece);
                }
                // Mirrored for the live view. An empty delta appends nothing to
                // either, so a provider that sends one — DeepSeek does — cannot
                // make what is on screen flicker.
                self.reasoning.push_str(&piece);
            }
            Some("signature_delta") => {
                if let Some(Partial::Thinking { signature, .. }) = self.at(index) {
                    *signature = Some(field("signature"));
                }
            }
            Some("input_json_delta") => {
                let piece = field("partial_json");
                self.output_bytes = self.output_bytes.saturating_add(piece.len() as u64);
                if let Some(Partial::Tool { json, .. }) = self.at(index) {
                    json.push_str(&piece);
                }
            }
            _ => {}
        }
    }

    /// Closes a block, which is where a tool call's arguments become usable.
    fn block_stop(&mut self, event: &Value) {
        let index = index_of(event);
        if let Some(Partial::Tool { json, opened, .. }) = self.at(index) {
            if json.is_empty() {
                return;
            }
            // Parsed here rather than in `finish` so the error names the block
            // it came from while the index is still to hand.
            match serde_json::from_str(json) {
                Ok(value) => *opened = Some(value),
                Err(e) => log::warn!("a streamed tool call's arguments did not decode: {e}"),
            }
        }
    }

    fn message_delta(&mut self, event: &Value) {
        if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_owned());
        }
        // The closing count of output tokens arrives here, the input side with
        // `message_start`; merging rather than replacing keeps whichever the
        // provider chose to send.
        if let Some(usage) = event.get("usage") {
            self.usage.merge(&usage_of(usage));
        }
    }

    /// The block at `index`, if one is open there.
    fn at(&mut self, index: usize) -> Option<&mut Partial> {
        self.blocks.get_mut(index).and_then(Option::as_mut)
    }

    fn put(&mut self, index: usize, block: Partial) {
        if index >= self.blocks.len() {
            self.blocks.resize_with(index + 1, || None);
        }
        if let Some(slot) = self.blocks.get_mut(index) {
            *slot = Some(block);
        }
    }

    /// The finished reply.
    ///
    /// # Errors
    /// Returns a description when the stream ended before `message_stop`, which
    /// is a connection that dropped mid-reply. That has to be an error rather
    /// than a short answer: a truncated stream and a model that stopped talking
    /// look identical once reassembled, and only one of them is a turn.
    pub fn finish(self) -> Result<MessagesResp, String> {
        if !self.done {
            return Err("the streamed reply ended before `message_stop`".to_owned());
        }
        let mut content = Vec::new();
        for block in self.blocks.into_iter().flatten() {
            match block {
                // An empty text block is the opening frame of a block that then
                // received nothing. Pushing it would put a blank block in the
                // history, which some providers reject on the way back.
                Partial::Text(text) if text.is_empty() => {}
                Partial::Text(text) => content.push(Block::Text { text }),
                Partial::Thinking { text, signature } => {
                    content.push(Block::Thinking {
                        thinking: text,
                        signature,
                    });
                }
                Partial::Tool { id, name, opened, .. } => content.push(Block::ToolUse {
                    id,
                    name,
                    input: opened.unwrap_or_else(|| Value::Object(Map::new())),
                }),
            }
        }
        Ok(MessagesResp {
            content,
            model: self.model,
            stop_reason: self.stop_reason,
            usage: self.usage,
        })
    }
}

impl Usage {
    /// Take the non-zero counts from a later report of the same usage.
    ///
    /// The input side arrives with `message_start` and the output side with
    /// `message_delta`, so the second report describes only part of the bill.
    /// Taking the larger of each pair would be wrong for a cache field that
    /// legitimately stops being reported; taking the non-zero one is not — a
    /// count that is absent is not a count that fell to zero.
    fn merge(&mut self, later: &Self) {
        let pairs = [
            (&mut self.input_tokens, later.input_tokens),
            (&mut self.output_tokens, later.output_tokens),
            (&mut self.cache_read_input_tokens, later.cache_read_input_tokens),
            (&mut self.cache_creation_input_tokens, later.cache_creation_input_tokens),
        ];
        for (mine, theirs) in pairs {
            if theirs > 0 {
                *mine = theirs;
            }
        }
    }
}

/// The `index` an event addresses, defaulting to the first block.
///
/// A provider that sends one block and omits the index means the first one;
/// treating a missing index as "no block" would drop its text.
fn index_of(event: &Value) -> usize {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// The token counts from a `usage` object.
fn usage_of(usage: &Value) -> Usage {
    let count = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| usage.get(*name).and_then(Value::as_u64))
            .unwrap_or(0)
    };
    Usage {
        input_tokens: count(&["input_tokens", "prompt_tokens"]),
        output_tokens: count(&["output_tokens", "completion_tokens"]),
        cache_read_input_tokens: count(&["cache_read_input_tokens", "cache_read_tokens", "cached_tokens"]),
        cache_creation_input_tokens: count(&["cache_creation_input_tokens", "cache_write_tokens"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One SSE frame, as the provider writes it.
    fn frame(kind: &str, data: &Value) -> String {
        format!("event: {kind}\ndata: {data}\n\n")
    }

    /// A stream of frames, fed a few bytes at a time.
    fn feed_all(asm: &mut Assembler, text: &str, step: usize) {
        for piece in text.as_bytes().chunks(step) {
            asm.feed(piece).expect("a well-formed stream feeds");
        }
    }

    fn text_delta(index: usize, text: &str) -> String {
        frame(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        )
    }

    fn thinking_delta(index: usize, text: &str) -> String {
        frame(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "thinking_delta", "thinking": text},
            }),
        )
    }

    fn tool_delta(index: usize, json: &str) -> String {
        frame(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": json},
            }),
        )
    }

    fn open_block(index: usize, block: &Value) -> String {
        frame(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }),
        )
    }

    fn close_block(index: usize) -> String {
        frame(
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": index}),
        )
    }

    const START: &str = concat!(
        r#"event: message_start
data: {"type":"message_start","message":{"model":"deepseek-flash","#,
        r#""usage":{"input_tokens":11,"cache_read_input_tokens":3}}}

"#,
    );

    const STOP: &str = concat!(
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"#,
        r#""usage":{"output_tokens":7}}

event: message_stop
data: {"type":"message_stop"}

"#,
    );

    /// The whole point: reasoning is readable while the reply is still coming.
    #[test]
    fn reasoning_is_available_before_the_reply_finishes() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        assert_eq!(asm.reasoning(), "", "nothing said yet");

        asm.feed(open_block(0, &serde_json::json!({"type": "thinking", "thinking": ""})).as_bytes())
            .expect("open");
        asm.feed(thinking_delta(0, "The user said hi. ").as_bytes())
            .expect("delta");
        // Read mid-stream, which is the state the live view draws.
        assert_eq!(asm.reasoning(), "The user said hi. ");
        assert!(!asm.is_done());

        asm.feed(thinking_delta(0, "Keep it short.").as_bytes()).expect("delta");
        assert_eq!(asm.reasoning(), "The user said hi. Keep it short.");

        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");
        assert!(asm.is_done());

        let resp = asm.finish().expect("a complete stream finishes");
        assert_eq!(resp.model, "deepseek-flash");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        // The two reports of usage are merged rather than one replacing the
        // other: the input side came with `message_start`, the output side here.
        assert_eq!(resp.usage.input_tokens, 11);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.usage.cache_read_input_tokens, 3);
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            Block::Thinking { thinking, .. } => {
                assert_eq!(thinking, "The user said hi. Keep it short.");
            }
            other => panic!("expected the thinking block, got {other:?}"),
        }
    }

    /// The counts the live view shows: available from `message_start`, and the output figure it
    /// carries is the provider's final one rather than a running total.
    ///
    /// The asymmetry is the reason the live view needs [`Assembler::output_bytes`] at all. The
    /// input count is complete the moment the reply opens; the output count arrives once, at the
    /// close, and reading zero until then is what made the status row sit on the request's own
    /// size for the whole generation.
    #[test]
    fn usage_is_readable_from_the_head_of_the_reply() {
        let mut asm = Assembler::new();
        assert!(asm.usage().is_none(), "nothing reported before the reply begins");
        assert!(asm.model().is_none());
        assert!(!asm.started());

        asm.feed(START.as_bytes()).expect("start");
        let usage = asm.usage().expect("reported with message_start");
        assert_eq!(usage.input_tokens, 11, "the input side is complete at the head");
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.output_tokens, 0, "the provider has not counted output yet");
        assert_eq!(asm.model(), Some("deepseek-flash"), "and the model is named here too");
        assert!(asm.started());
    }

    /// The count that moves while the model writes: every kind of content delta adds to it.
    ///
    /// Text, reasoning and tool arguments all bill as output, so a figure that watched only one
    /// of them would jump backwards when the model moved from thinking to calling a tool.
    ///
    /// The expected figures are taken from the strings fed in rather than written as literals:
    /// this is a byte count, and a hand-counted third of it is a test that passes for the wrong
    /// reason the next time a string is retyped.
    #[test]
    fn output_bytes_count_reasoning_text_and_tool_arguments() {
        let reasoning = "weigh it up";
        let text = "Here.";
        let arguments = r#"{"file_path":"a.rs"}"#;
        let mut asm = Assembler::new();
        assert_eq!(asm.output_bytes(), 0, "a reply that has not begun has produced nothing");

        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "thinking", "thinking": ""})).as_bytes())
            .expect("open thinking");
        asm.feed(thinking_delta(0, reasoning).as_bytes()).expect("delta");
        assert_eq!(asm.output_bytes(), reasoning.len() as u64);

        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(open_block(1, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open text");
        asm.feed(text_delta(1, text).as_bytes()).expect("delta");
        assert_eq!(
            asm.output_bytes(),
            (reasoning.len() + text.len()) as u64,
            "the reasoning is not discarded when text starts"
        );

        asm.feed(close_block(1).as_bytes()).expect("close");
        asm.feed(
            open_block(
                2,
                &serde_json::json!({"type": "tool_use", "id": "t", "name": "Read", "input": {}}),
            )
            .as_bytes(),
        )
        .expect("open tool");
        asm.feed(tool_delta(2, arguments).as_bytes()).expect("delta");
        assert_eq!(
            asm.output_bytes(),
            (reasoning.len() + text.len() + arguments.len()) as u64
        );
    }

    /// `DeepSeek` sends empty deltas, and they must not disturb the view.
    #[test]
    fn an_empty_thinking_delta_changes_nothing() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "thinking", "thinking": ""})).as_bytes())
            .expect("open");
        asm.feed(thinking_delta(0, "Thinking").as_bytes()).expect("delta");
        let before = asm.reasoning().to_owned();
        // A real transcript has exactly this block.
        asm.feed(thinking_delta(0, "").as_bytes()).expect("delta");
        assert_eq!(asm.reasoning(), before, "an empty delta is a no-op");
    }

    /// A reply that is text and nothing else, the ordinary case.
    #[test]
    fn a_plain_text_reply_needs_no_thinking() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open");
        asm.feed(text_delta(0, "Hello").as_bytes()).expect("delta");
        asm.feed(text_delta(0, " there").as_bytes()).expect("delta");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");
        assert_eq!(asm.reasoning(), "", "text is not reasoning");

        let resp = asm.finish().expect("finish");
        match &resp.content[0] {
            Block::Text { text } => assert_eq!(text, "Hello there"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// A block that opened and received nothing must not reach the history.
    #[test]
    fn an_empty_text_block_is_dropped() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");
        assert!(asm.finish().expect("finish").content.is_empty());
    }

    /// The relay sends a tool call's whole argument object as one delta, after
    /// opening the block with an empty one.
    #[test]
    fn a_tool_call_is_assembled_from_its_argument_deltas() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(
            open_block(
                0,
                &serde_json::json!({"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {}}),
            )
            .as_bytes(),
        )
        .expect("open");
        asm.feed(tool_delta(0, r#"{"command":"ls -la"}"#).as_bytes())
            .expect("delta");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");

        let resp = asm.finish().expect("finish");
        match &resp.content[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls -la");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// A tool that takes no arguments sends `{}` at the open and no delta, which
    /// must not become a decode failure.
    #[test]
    fn a_tool_call_with_no_arguments_keeps_the_empty_object() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(
            open_block(
                0,
                &serde_json::json!({"type": "tool_use", "id": "tu_2", "name": "Todo", "input": {}}),
            )
            .as_bytes(),
        )
        .expect("open");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");

        match &asm.finish().expect("finish").content[0] {
            Block::ToolUse { input, .. } => assert_eq!(input, &serde_json::json!({})),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// A provider may inline the whole argument object and send no deltas.
    #[test]
    fn a_tool_call_whose_arguments_arrived_whole_is_kept() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(
            open_block(
                0,
                &serde_json::json!({
                    "type": "tool_use",
                    "id": "tu_3",
                    "name": "Read",
                    "input": {"file_path": "/tmp/x"},
                }),
            )
            .as_bytes(),
        )
        .expect("open");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");

        match &asm.finish().expect("finish").content[0] {
            Block::ToolUse { input, .. } => assert_eq!(input["file_path"], "/tmp/x"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// A chunk boundary can fall anywhere, including inside a character.
    #[test]
    fn a_reply_split_at_every_boundary_still_assembles() {
        // Built by appending rather than with a format string: the point of the
        // test is the framing, and a miscounted placeholder would be a bug in
        // the test rather than in the code under it.
        let mut whole = String::from(START);
        whole.push_str(&open_block(0, &serde_json::json!({"type": "thinking", "thinking": ""})));
        whole.push_str(&thinking_delta(0, "Pondering \u{2014} with an em dash "));
        whole.push_str(&thinking_delta(0, "and \u{2713} a tick"));
        whole.push_str(&close_block(0));
        // The answer is a second block, at its own index. A delta that names the
        // index of a block of another kind is not applied — which is what a
        // miscounted index here would have silently proved.
        whole.push_str(&open_block(1, &serde_json::json!({"type": "text", "text": ""})));
        whole.push_str(&text_delta(1, "Answer \u{1F600}"));
        whole.push_str(&close_block(1));
        whole.push_str(STOP);
        // One byte at a time is the worst case: every multi-byte character is
        // split, and so is every JSON escape and every frame boundary.
        for step in [1, 2, 3, 7, 64, whole.len()] {
            let mut asm = Assembler::new();
            feed_all(&mut asm, &whole, step);
            let resp = asm.finish().unwrap_or_else(|e| panic!("step {step}: {e}"));
            assert_eq!(resp.content.len(), 2, "step {step}");
            match (&resp.content[0], &resp.content[1]) {
                (Block::Thinking { thinking, .. }, Block::Text { text }) => {
                    assert_eq!(thinking, "Pondering \u{2014} with an em dash and \u{2713} a tick");
                    assert_eq!(text, "Answer \u{1F600}");
                }
                other => panic!("step {step}: unexpected blocks {other:?}"),
            }
        }
    }

    /// A dropped connection must not look like a model that had finished.
    #[test]
    fn a_stream_that_stops_early_is_an_error_not_a_short_answer() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(text_delta(0, "half an ans").as_bytes()).expect("delta");
        let err = asm.finish().expect_err("no message_stop means truncated");
        assert!(err.contains("message_stop"), "{err}");
    }

    /// The provider's own error event is reported with its message.
    #[test]
    fn an_error_event_is_reported_with_its_message() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        let err = asm
            .feed(
                frame(
                    "error",
                    &serde_json::json!({
                        "type": "error",
                        "error": {"type": "overloaded_error", "message": "Overloaded"},
                    }),
                )
                .as_bytes(),
            )
            .expect_err("an error event is an error");
        assert_eq!(err, "Overloaded");
    }

    /// `ping`, unknown events, the `[DONE]` sentinel and CRLF are all survived.
    #[test]
    fn keep_alives_and_unknown_events_are_ignored() {
        let mut asm = Assembler::new();
        asm.feed(b": a comment\n\n").expect("comment");
        asm.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\n").expect("ping");
        asm.feed(b"event: something_new\ndata: {\"type\":\"future_thing\"}\n\n")
            .expect("unknown");
        asm.feed(b"data: [DONE]\n\n").expect("sentinel");
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open");
        asm.feed(text_delta(0, "ok").as_bytes()).expect("delta");
        asm.feed(close_block(0).as_bytes()).expect("close");
        // CRLF rather than LF, which must not leave a stray carriage return.
        asm.feed(STOP.replace('\n', "\r\n").as_bytes()).expect("stop");
        assert_eq!(asm.finish().expect("finish").content.len(), 1);
    }

    /// A payload that is not JSON is reported rather than ignored, because a
    /// frame we cannot read may have been the one carrying the answer.
    #[test]
    fn a_payload_that_is_not_json_is_an_error() {
        let mut asm = Assembler::new();
        let err = asm.feed(b"data: not json at all\n\n").expect_err("garbage payload");
        assert!(err.contains("not JSON"), "{err}");
    }

    /// Blocks are placed by the index the deltas address, not by arrival order.
    #[test]
    fn blocks_land_at_the_index_they_name() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open 0");
        asm.feed(open_block(1, &serde_json::json!({"type": "text", "text": ""})).as_bytes())
            .expect("open 1");
        asm.feed(text_delta(1, "second").as_bytes()).expect("delta 1");
        asm.feed(text_delta(0, "first").as_bytes()).expect("delta 0");
        asm.feed(close_block(0).as_bytes()).expect("close 0");
        asm.feed(close_block(1).as_bytes()).expect("close 1");
        asm.feed(STOP.as_bytes()).expect("stop");

        let resp = asm.finish().expect("finish");
        match (&resp.content[0], &resp.content[1]) {
            (Block::Text { text: a }, Block::Text { text: b }) => {
                assert_eq!(a, "first");
                assert_eq!(b, "second");
            }
            other => panic!("blocks are ordered by index, got {other:?}"),
        }
    }

    /// A signature that arrives as a delta is kept on the block.
    #[test]
    fn a_signature_delta_is_kept_on_its_thinking_block() {
        let mut asm = Assembler::new();
        asm.feed(START.as_bytes()).expect("start");
        asm.feed(open_block(0, &serde_json::json!({"type": "thinking", "thinking": ""})).as_bytes())
            .expect("open");
        asm.feed(thinking_delta(0, "hm").as_bytes()).expect("delta");
        asm.feed(
            frame(
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-abc"},
                }),
            )
            .as_bytes(),
        )
        .expect("signature");
        asm.feed(close_block(0).as_bytes()).expect("close");
        asm.feed(STOP.as_bytes()).expect("stop");

        match &asm.finish().expect("finish").content[0] {
            Block::Thinking { signature, .. } => {
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
    }
}
