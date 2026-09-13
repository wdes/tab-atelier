// SPDX-License-Identifier: MPL-2.0

//! What a request claims to be, before a non-Anthropic model reads it.
//!
//! Claude Code's system prompt opens by naming its client and carries
//! Anthropic's own billing header, and its git instructions end a commit with a
//! `Co-Authored-By: Claude` trailer. None of that is true when the model on the
//! far end is `DeepSeek`'s or `OpenAI`'s: the prompt tells that model it is another
//! vendor's product, Anthropic's telemetry travels to a competitor, and the
//! commits it writes credit a model that had no part in them.
//!
//! So a request bound for anyone but Anthropic is rewritten first: the
//! `x-anthropic-*` header line goes, the client's name goes, and the trailer
//! names the vendor that actually answers. Anthropic's own requests are left
//! alone, because for them the claims are true.
//!
//! The rewrite is confined to `system`. The same strings appear in what the
//! user typed and pasted, and editing those would corrupt the conversation — a
//! person asking about `Claude Code` by name would have the words deleted
//! before the model ever saw the question.

use serde_json::Value;

use crate::provider::Provider;

/// Who a request is really going to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// Anthropic itself, where nothing is rewritten.
    Anthropic,
    Deepseek,
    Openai,
    /// Some other non-Anthropic destination. The Anthropic identity is removed,
    /// but there is no vendor to credit a commit to, so the trailer is left.
    Other,
}

impl Vendor {
    /// Classify a provider by what it is, not by the wire it speaks.
    ///
    /// The wire will not do: `DeepSeek` answers on the Anthropic-shaped endpoint,
    /// so `Wire::Anthropic` covers both the real Anthropic and the vendor whose
    /// model must never be told it is Claude. The id and the base URL are what
    /// distinguish them, and the base URL is checked too so a renamed provider
    /// keeps its vendor.
    #[must_use]
    pub fn of(provider: &Provider) -> Self {
        let id = provider.id.to_ascii_lowercase();
        let base = provider.base_url.to_ascii_lowercase();
        if id.contains("anthropic") || base.contains("anthropic.") {
            Self::Anthropic
        } else if id.contains("deepseek") || base.contains("deepseek") {
            Self::Deepseek
        } else if id.contains("openai") || base.contains("openai") {
            Self::Openai
        } else {
            Self::Other
        }
    }

    /// The trailer line to credit instead, or `None` to leave the original.
    ///
    /// `DeepSeek` and `OpenAI` name their vendor; `OpenAI` also names the model,
    /// because its provider serves several and the commit should say which one
    /// wrote it.
    fn trailer(self, model: &str) -> Option<String> {
        match self {
            Self::Anthropic | Self::Other => None,
            Self::Deepseek => Some("Co-authored-by: DeepSeek <noreply@deepseek.com>".to_owned()),
            Self::Openai => Some(format!("Co-authored-by: OpenAI {model} <noreply@openai.com>")),
        }
    }
}

/// Rewrite the system prompt of `body` for `vendor`.
pub fn apply(body: &mut Value, vendor: Vendor, model: &str) {
    if vendor == Vendor::Anthropic {
        return;
    }
    let trailer = vendor.trailer(model);
    let Some(system) = body.get_mut("system") else {
        return;
    };
    match system {
        Value::String(text) => *text = rewrite(text, trailer.as_deref()),
        // A system prompt is a string or a list of text blocks. Both spellings
        // are in use, and a client is free to pick either.
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(Value::String(text)) = block.get_mut("text") {
                    *text = rewrite(text, trailer.as_deref());
                }
            }
        }
        _ => {}
    }
}

/// Whether a body could contain anything [`apply`] would change.
///
/// Every request from Claude Code carries a system prompt naming the client, so
/// this saves the parse only for the proxy's own calls — the classifier and the
/// title generator, whose prompts are written here and mention none of it. The
/// needles must cover every edit `apply` makes, or it would start skipping work
/// it owes.
#[must_use]
pub fn mentions(body: &[u8]) -> bool {
    const NEEDLES: [&[u8]; 3] = [b"Claude Code", b"x-anthropic-", b"uthored-by:"];
    NEEDLES
        .iter()
        .any(|needle| body.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle)))
}

fn rewrite(text: &str, trailer: Option<&str>) -> String {
    let text = strip_x_anthropic(text);
    let text = text.replace("Claude Code", "");
    trailer.map_or_else(|| text.clone(), |line| rewrite_trailer(&text, line))
}

/// Drop the whole line of any `x-anthropic-*` header.
///
/// Claude Code prepends its billing header — `x-anthropic-billing-header:` and
/// a version, an entrypoint, a checksum — as the first line of the system
/// prompt. It is addressed to Anthropic, so it does not travel to anyone else.
fn strip_x_anthropic(text: &str) -> String {
    const PREFIX: &[u8] = b"x-anthropic-";
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let head = line.trim_start().as_bytes();
        if head.get(..PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(PREFIX)) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Replace each `Co-Authored-By: … <…>` on its line with `replacement`.
///
/// Scanned rather than pattern-matched: the trailer is a line of ASCII in a
/// body that may be megabytes, and one pass over the lines costs less than a
/// regex engine entering the dependency tree.
fn rewrite_trailer(text: &str, replacement: &str) -> String {
    const NEEDLE: &[u8] = b"co-authored-by:";
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let Some(start) = find_ignoring_ascii_case(line, NEEDLE).filter(|s| line.is_char_boundary(*s)) else {
            out.push_str(line);
            continue;
        };
        // The name sits between `<` and `>`; a line mentioning the trailer
        // without both is prose about it, not a trailer, and is left alone.
        let (Some(open), Some(close)) = (line[start..].find('<'), line[start..].find('>')) else {
            out.push_str(line);
            continue;
        };
        if open >= close {
            out.push_str(line);
            continue;
        }
        out.push_str(&line[..start]);
        out.push_str(replacement);
        out.push_str(&line[start + close + 1..]);
    }
    out
}

fn find_ignoring_ascii_case(haystack: &str, needle: &[u8]) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(id: &str, base_url: &str) -> Provider {
        Provider {
            id: id.to_owned(),
            wire: crate::provider::Wire::Anthropic,
            base_url: base_url.to_owned(),
            auth: crate::provider::Auth::ApiKeyEnv { var: "STUB".to_owned() },
            models: Vec::new(),
            preference: 0,
            enabled: true,
            peak: None,
        }
    }

    #[test]
    fn the_wire_does_not_decide_the_vendor() {
        // DeepSeek answers on the Anthropic wire; a rule keyed on the wire
        // would treat it as Anthropic and rewrite nothing.
        assert_eq!(
            Vendor::of(&provider("deepseek", "https://api.deepseek.com/anthropic")),
            Vendor::Deepseek
        );
        assert_eq!(
            Vendor::of(&provider("openai", "https://api.openai.com/v1")),
            Vendor::Openai
        );
        assert_eq!(
            Vendor::of(&provider("subscription", "https://api.anthropic.com")),
            Vendor::Anthropic
        );
        // A renamed provider keeps its vendor through the base URL.
        assert_eq!(
            Vendor::of(&provider("fast", "https://api.deepseek.com/anthropic")),
            Vendor::Deepseek
        );
        assert_eq!(Vendor::of(&provider("local", "http://127.0.0.1:8080")), Vendor::Other);
    }

    #[test]
    fn anthropic_is_left_exactly_as_it_arrived() {
        let mut body = json!({
            "system": "x-anthropic-billing-header: cc_version=1.2.3;\nYou are Claude Code, Anthropic's official CLI for Claude.",
        });
        let before = body.clone();
        apply(&mut body, Vendor::Anthropic, "claude-opus-4-8");
        assert_eq!(body, before);
    }

    #[test]
    fn the_billing_header_line_goes_and_the_client_name_goes() {
        let mut body = json!({
            "system": "x-anthropic-billing-header: cc_version=2.1.167; cc_entrypoint=cli;\n\
                       You are Claude Code, Anthropic's official CLI for Claude.\n\
                       Keep going.",
        });
        apply(&mut body, Vendor::Openai, "gpt-5.6-luna");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("x-anthropic-"), "{text}");
        assert!(!text.contains("Claude Code"), "{text}");
        assert!(text.contains("Keep going."), "{text}");
    }

    #[test]
    fn each_vendor_is_credited_in_its_own_trailer() {
        let system = "End git commit messages with:\nCo-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>";
        let rewritten = |vendor, model| {
            let mut body = json!({ "system": system });
            apply(&mut body, vendor, model);
            body["system"].as_str().unwrap().to_owned()
        };
        assert!(
            rewritten(Vendor::Deepseek, "deepseek-flash").contains("Co-authored-by: DeepSeek <noreply@deepseek.com>")
        );
        assert!(
            rewritten(Vendor::Openai, "gpt-5.6-luna")
                .contains("Co-authored-by: OpenAI gpt-5.6-luna <noreply@openai.com>")
        );
        // Nothing to credit an unknown vendor to, so the line is left alone.
        assert!(rewritten(Vendor::Other, "x").contains("Co-Authored-By: Claude Opus 4.8"));
    }

    #[test]
    fn a_lowercase_trailer_and_a_block_system_are_both_handled() {
        let mut body = json!({
            "system": [
                { "type": "text", "text": "co-authored-by: Claude <noreply@anthropic.com>" },
                { "type": "text", "text": "untouched" },
            ],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        assert_eq!(
            body["system"][0]["text"],
            "Co-authored-by: DeepSeek <noreply@deepseek.com>"
        );
        assert_eq!(body["system"][1]["text"], "untouched");
    }

    #[test]
    fn the_needles_cover_every_edit() {
        // A body the guard skips must be one `apply` would not have changed.
        assert!(mentions(b"You are Claude Code, Anthropic's official CLI"));
        assert!(mentions(b"x-anthropic-billing-header: cc_version=1"));
        assert!(mentions(b"Co-Authored-By: Claude <noreply@anthropic.com>"));
        assert!(!mentions(b"{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"));
    }
}
