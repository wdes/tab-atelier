// SPDX-License-Identifier: MPL-2.0

//! Documents a vendor will not take, replaced before they cost the whole request.
//!
//! Claude Code attaches a PDF it has read as a `document` block with a `base64`
//! source. `DeepSeek`'s Anthropic-shaped endpoint accepts a document only from a
//! `text` or a `file` source and answers a 400 for the entire body otherwise:
//!
//! > messages.300.content.1.source: only `file` and `text` document sources are
//! > supported
//!
//! The block is history by the next turn, so the refusal is not one failed
//! request but every request the session makes from then on. A note standing
//! where the document stood keeps the session alive, and tells the model the
//! truth — that something was attached and it cannot see it — so it can reach
//! for the file another way instead of answering as if nothing were there.
//!
//! The note is a pure function of the block, so the same history shapes to the
//! same bytes on every turn and the provider's prefix cache is not disturbed.

use serde_json::{Value, json};

/// Replace every document `DeepSeek` would refuse. Returns how many were.
pub fn drop_unreadable_documents(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    messages
        .iter_mut()
        .filter_map(|message| message.get_mut("content").and_then(Value::as_array_mut))
        .map(|blocks| replace_in(blocks))
        .sum()
}

fn replace_in(blocks: &mut [Value]) -> usize {
    let mut replaced = 0;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("document") if !is_readable(block) => {
                *block = note(block);
                replaced += 1;
            }
            // A tool can hand a document back inside its result.
            Some("tool_result") => {
                if let Some(inner) = block.get_mut("content").and_then(Value::as_array_mut) {
                    replaced += replace_in(inner);
                }
            }
            _ => {}
        }
    }
    replaced
}

fn is_readable(document: &Value) -> bool {
    matches!(
        document
            .get("source")
            .and_then(|source| source.get("type"))
            .and_then(Value::as_str),
        Some("text" | "file")
    )
}

fn note(document: &Value) -> Value {
    let source = document.get("source");
    let field = |name: &str| source.and_then(|s| s.get(name)).and_then(Value::as_str);
    let mut what = field("media_type").unwrap_or("document").to_owned();
    if let Some(title) = document.get("title").and_then(Value::as_str) {
        what = format!("{what} \"{title}\"");
    }
    if let Some(data) = field("data") {
        // Base64 carries three bytes in four characters.
        what = format!("{what}, about {} bytes", data.len() / 4 * 3);
    }
    let mut note = json!({
        "type": "text",
        "text": format!(
            "[A document was attached here ({what}) but the model provider serving this conversation \
             cannot read attached documents, so its content was not sent. If you need it, extract its \
             text with a tool instead.]"
        ),
    });
    // A cache breakpoint marks a position in the prompt, not this block's kind.
    if let Some(cache) = document.get("cache_control") {
        note["cache_control"] = cache.clone();
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf() -> Value {
        json!({
            "type": "document",
            "source": { "type": "base64", "media_type": "application/pdf", "data": "AAAABBBB" },
        })
    }

    /// The reported failure: a PDF in history made `DeepSeek` refuse every turn.
    #[test]
    fn a_base64_pdf_becomes_a_note() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "read this"}, pdf()]},
        ]});
        assert_eq!(drop_unreadable_documents(&mut body), 1);
        let block = &body["messages"][0]["content"][1];
        assert_eq!(block["type"], "text");
        let text = block["text"].as_str().expect("text");
        assert!(text.contains("application/pdf") && text.contains("6 bytes"), "{text}");
        assert!(block.get("source").is_none(), "the payload is gone");
    }

    #[test]
    fn the_sources_deepseek_accepts_are_left_alone() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "hi"}},
            {"type": "document", "source": {"type": "file", "file_id": "file_1"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
        ]}]});
        let before = body.clone();
        assert_eq!(drop_unreadable_documents(&mut body), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn a_document_inside_a_tool_result_is_found() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [pdf()]},
        ]}]});
        assert_eq!(drop_unreadable_documents(&mut body), 1);
        assert_eq!(body["messages"][0]["content"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn the_cache_breakpoint_stays_where_it_was() {
        let mut document = pdf();
        document["cache_control"] = json!({"type": "ephemeral"});
        let mut body = json!({"messages": [{"role": "user", "content": [document]}]});
        drop_unreadable_documents(&mut body);
        assert_eq!(body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn string_content_and_a_missing_messages_array_are_left_alone() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(drop_unreadable_documents(&mut body), 0);
        let mut body = json!({"model": "x"});
        assert_eq!(drop_unreadable_documents(&mut body), 0);
    }
}
