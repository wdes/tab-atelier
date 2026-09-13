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
//! So a request bound for anyone but Anthropic is rewritten first. The
//! `x-anthropic-*` header line goes; the identity sentence and the PR-body
//! instruction are dropped whole; what remains of the client's name goes; and
//! the trailer names the vendor that actually answers. Anthropic's own requests
//! are left alone, because for them the claims are true.
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

fn rewrite(text: &str, trailer: Option<&str>) -> String {
    let text = strip_dropped_lines(text);
    let text = text.replace("Claude Code", "");
    trailer.map_or_else(|| text.clone(), |line| rewrite_trailer(&text, line))
}

/// Drop the lines the far end has no business receiving.
///
/// Three kinds, all Anthropic's own:
///
/// * the `x-anthropic-*` billing header — a version, an entrypoint, a
///   checksum, addressed to Anthropic;
/// * the identity sentence, dropped whole. Removing only `Claude Code` from it
///   would leave `You are , Anthropic's official CLI for Claude`, and the claim
///   would survive the edit that exists to remove it;
/// * the PR-body instruction, label and attribution together. Dropping the
///   attribution alone would leave an instruction to end PR bodies with
///   nothing.
fn strip_dropped_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if dropped(line) {
            continue;
        }
        out.push_str(line);
    }
    out
}

fn dropped(line: &str) -> bool {
    let head = line.trim_start();
    // Both prompt blocks are list items, so a bullet may sit in front.
    let head = head.strip_prefix('-').map_or(head, str::trim_start);
    let lower = head.to_ascii_lowercase();
    lower.starts_with("x-anthropic-")
        || lower.contains("you are claude code, anthropic's official cli for claude")
        || lower.contains("generated with [claude code]")
        || lower.starts_with("end pr bodies with")
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
    fn the_identity_sentence_is_dropped_whole_not_gutted() {
        let mut body = json!({
            "system": "You are Claude Code, Anthropic's official CLI for Claude.\n\nYou are an agent.",
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        // Removing only `Claude Code` would leave the claim standing.
        assert!(!text.contains("Anthropic's official CLI for Claude"), "{text}");
        assert!(!text.contains("Claude Code"), "{text}");
        assert!(text.contains("You are an agent."), "{text}");
    }

    #[test]
    fn the_pr_body_block_goes_label_and_attribution_together() {
        let mut body = json!({
            "system": "Preface.\n\
                       - End PR bodies with:\n\
                       🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\
                       Epilogue.",
        });
        apply(&mut body, Vendor::Openai, "gpt-5.6-luna");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("End PR bodies"), "{text}");
        assert!(!text.to_lowercase().contains("generated with"), "{text}");
        assert!(text.contains("Preface."), "{text}");
        assert!(text.contains("Epilogue."), "{text}");
    }

    #[test]
    fn a_commit_message_body_is_not_mistaken_for_the_instruction() {
        // The rule drops the *instruction*, matched from the start of the line.
        // A commit message that merely mentions the phrase mid-sentence is prose.
        let mut body = json!({ "system": "Write it so you can end PR bodies with a table." });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        assert!(body["system"].as_str().unwrap().contains("end PR bodies with a table."));
    }
}
