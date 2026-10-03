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

/// Rewrite the request's prose for `vendor`.
///
/// Two places carry it, and the second is the one that bites: the system
/// prompt, and the tool definitions. `Claude Code` writes its own commit and PR
/// conventions into the `Bash` tool's description — `Co-Authored-By: Claude
/// …` among them — so a rewrite that walks `system` alone leaves the
/// attribution sitting in `tools[].description`, untouched and sent to a
/// competitor's model. Tool *names* are never touched; only prose is.
///
/// One rule is conditional rather than textual: the sentence forbidding the
/// Agent tool is dropped only when the body no longer offers one. That is a
/// question about `tools[]`, so it is asked here — this runs after the tool
/// policy, on the list that will actually be sent.
pub fn apply(body: &mut Value, vendor: Vendor, model: &str) {
    if vendor == Vendor::Anthropic {
        return;
    }
    let trailer = vendor.trailer(model);
    let drop_agent_rule = agent_rule_is_dangling(body);
    if let Some(system) = body.get_mut("system") {
        match system {
            Value::String(text) => {
                *text = system_rewrite(text, trailer.as_deref(), model, drop_agent_rule);
            }
            // A system prompt is a string or a list of text blocks. Both
            // spellings are in use, and a client is free to pick either.
            Value::Array(blocks) => {
                for block in blocks {
                    if let Some(Value::String(text)) = block.get_mut("text") {
                        *text = system_rewrite(text, trailer.as_deref(), model, drop_agent_rule);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(tools) = body.get_mut("tools") {
        rewrite_descriptions(tools, trailer.as_deref(), model);
    }
}

/// `system` text with the unconditional rules applied, then the Agent sentence
/// removed if it has nothing left to govern.
fn system_rewrite(text: &str, trailer: Option<&str>, model: &str, drop_agent_rule: bool) -> String {
    let text = rewrite(text, trailer, model);
    if drop_agent_rule { strip_agent_rule(&text) } else { text }
}

/// The tools the "do not use the Agent tool…" sentence governs.
///
/// `Agent` is the current name and `Task` the former one; both spellings ship.
/// `Workflow` and `DeepResearch` are guesses at the other two things the
/// sentence lists, and a name that never appears costs nothing here — it can
/// only ever count as absent. An unrecognized spelling is the one error that
/// matters, and it fails toward keeping the sentence.
const AGENT_RULE_TOOLS: [&str; 4] = ["Agent", "Task", "Workflow", "DeepResearch"];

/// Whether the sentence forbidding the Agent tool has nothing left to forbid.
///
/// Asked of the body rather than of the text, because the answer is about what
/// the tool policy sent, not about what the sentence says. A body with no
/// `tools[]` at all keeps the sentence: sending no tools is not evidence a tool
/// was removed, and a request that carries none was not written for this rule.
fn agent_rule_is_dangling(body: &Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    let names_a_governed_tool = |tool: &Value| {
        tool.get("name")
            .or_else(|| tool.pointer("/function/name"))
            .and_then(Value::as_str)
            .is_some_and(|name| AGENT_RULE_TOOLS.iter().any(|known| known.eq_ignore_ascii_case(name)))
    };
    !tools.iter().any(names_a_governed_tool)
}

/// Drop the sentence forbidding the Agent tool, whole lines only.
///
/// Matched on its opening clause: `do not use the agent tool` is specific to
/// this sentence, while its tail — `unless the user, a CLAUDE.md file, or a
/// skill asks for it` — is the shape of any permission rule and would
/// over-match. Lines are the unit, as everywhere else here.
fn strip_agent_rule(text: &str) -> String {
    const NEEDLE: &str = "do not use the agent tool";
    text.split_inclusive('\n')
        .filter(|line| !line.to_ascii_lowercase().contains(NEEDLE))
        .collect()
}

/// Rewrite every `description` string under `value`, at any depth.
///
/// Recursive because the prose is not only at the top: a tool's
/// `input_schema` describes each of its own properties, and those
/// descriptions are written by the same client. Keyed on the word
/// `description` rather than on a path, so it finds them both — and never
/// matches a `name`, an `enum` value or a `type`, which are the members a
/// rewrite would actually break.
fn rewrite_descriptions(value: &mut Value, trailer: Option<&str>, model: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "description"
                    && let Value::String(text) = child
                {
                    *text = rewrite(text, trailer, model);
                    continue;
                }
                rewrite_descriptions(child, trailer, model);
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_descriptions(item, trailer, model);
            }
        }
        _ => {}
    }
}

fn rewrite(text: &str, trailer: Option<&str>, model: &str) -> String {
    let text = strip_and_repoint(text, model);
    let text = text.replace("Claude Code", "");
    trailer.map_or_else(|| text.clone(), |line| rewrite_trailer(&text, line))
}

/// Drop the lines the far end has no business receiving, and repoint the one
/// that can simply be made true.
///
/// Dropped, all Anthropic's own:
///
/// * the `x-anthropic-*` billing header — a version, an entrypoint, a
///   checksum, addressed to Anthropic;
/// * the identity sentence, dropped whole. Removing only `Claude Code` from it
///   would leave `You are , Anthropic's official CLI for Claude`, and the claim
///   would survive the edit that exists to remove it;
/// * the PR-body instruction, label and attribution together. Dropping the
///   attribution alone would leave an instruction to end PR bodies with
///   nothing;
/// * the model catalogue and the line beside it. `When building AI
///   applications, default to the latest and most capable Claude models` is an
///   instruction to a competitor's model to promote Claude in the code it
///   writes, and the cutoff is a claim about Claude's training rather than this
///   model's;
/// * what the client *is* — a CLI on these platforms, a `fast` mode with these
///   models. True of the app, of no use to the work.
fn strip_and_repoint(text: &str, model: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match model_line(line, model) {
            Some(line) => out.push_str(&line),
            None if dropped(line) => {}
            None => out.push_str(line),
        }
    }
    out
}

/// The model line, rewritten to name the model that actually answers.
///
/// The one self-description worth keeping, because it can be made true rather
/// than only removed: routing already resolved the model, and the id it
/// resolved is what the far end will serve. Everything before the phrase is
/// kept, so the bullet survives.
fn model_line(line: &str, model: &str) -> Option<String> {
    const NEEDLE: &[u8] = b"you are powered by the model named";
    let at = find_ignoring_ascii_case(line, NEEDLE)?;
    if !line.is_char_boundary(at) {
        return None;
    }
    // The line terminator is part of the line: `split_inclusive` kept it, and
    // rebuilding without it would run this line into the next one. Only the
    // text after the needle is dropped, not the break that follows it.
    let end = &line[line.trim_end_matches('\n').len()..];
    Some(format!(
        "{}You are powered by the model named {model}.{end}",
        &line[..at]
    ))
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
        || lower.starts_with("assistant knowledge cutoff is")
        || lower.starts_with("the most recent claude model family is")
        || lower.starts_with("the most recent claude models are")
        || lower.starts_with("claude code is available as a")
        || lower.starts_with("fast mode for claude code")
        // The security-scope paragraph. Anthropic's policy, addressed to Claude
        // by name, and the only entry in this list that constrains behaviour
        // rather than branding. Dropping it is therefore not cosmetic: the
        // request goes on governed by the answering model's own policy, which
        // is the arrangement the operator chose by routing here in the first
        // place — not a second vendor's instructions carried along unread.
        //
        // Matched on the tail, not the head, unlike its neighbours. The head of
        // this one is generic — "IMPORTANT: …" is how anyone opens a directive,
        // so a `starts_with` on it would delete a real instruction that merely
        // began the same way, and would miss this one entirely if the client
        // prefixed it with a marker other than `-`. The clause at the end is
        // unique to this sentence, so quoting it in some other line is the only
        // way to over-match, and that is not a thing clients do.
        //
        // The client sends the paragraph as one unbroken line. Were it ever
        // hard-wrapped, no line would carry the tail and the opening line would
        // survive — the price of per-line matching, paid by every rule here.
        || lower.contains(
            "pentesting engagements, ctf competitions, security research, or defensive use cases",
        )
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
        // A line mentioning the trailer with no address at all is prose about
        // it, not a trailer, and is left alone.
        let Some(end) = closing_bracket(&line[start..]) else {
            out.push_str(line);
            continue;
        };
        out.push_str(&line[..start]);
        out.push_str(replacement);
        // Whatever closed the address is the last thing replaced; the rest of
        // the line, break included, is kept as it was.
        out.push_str(&line[start + end..]);
    }
    out
}

/// The byte just past the address in `rest`, if `rest` holds one.
///
/// A git trailer writes `Name <mail>`; the `Bash` tool's description of the
/// same convention writes `Name (mail)`. Both are shipped by the client, so
/// both have to close a trailer — matching only angle brackets is how the
/// `Co-Authored-By` line survived the rewrite that exists to remove it.
///
/// Either bracket only closes a trailer if it wraps an `@`: a line that merely
/// mentions the trailer, `Co-Authored-By: (see the docs)`, is prose about the
/// convention and is left alone. Every address has an `@`, so requiring one
/// costs no real trailer and buys back the false positive the second bracket
/// would otherwise introduce.
///
/// The search does not stop at the first bracket pair, because the first pair
/// need not be the address. A `Bash` description shipped by a later client
/// reads `Claude Opus 5 (1M context) <noreply@anthropic.com>` — name, then a
/// note about context, then the mail. Anchoring on the first pair found no `@`
/// in `(1M context)` and gave up, leaving the very trailer this exists to
/// remove. A bracket pair with no address is skipped, not terminal.
fn closing_bracket(rest: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(open) = rest[from..].find(['<', '(']).map(|o| o + from) {
        let close = rest[open..].find(['>', ')'])?;
        if rest[open + 1..open + close].contains('@') {
            return Some(open + close + 1);
        }
        from = open + 1;
    }
    None
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

    #[test]
    fn the_model_line_keeps_its_own_line_break() {
        // The instruction shares a line with the exact-model-ID sentence and is
        // followed by a break. Rebuilding it without that break ran it into the
        // next line the prune had let through, which is how it was noticed.
        let mut body = json!({
            "system": concat!(
                " - You are powered by the model named Opus 4.8. The exact model ID is claude-opus-4-8.\n",
                " - Assistant knowledge cutoff is January 2026.\n",
                " - You are an interactive agent.",
            ),
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        assert!(
            text.contains("You are powered by the model named deepseek-flash.\n"),
            "{text:?}"
        );
        assert!(!text.contains("claude-opus-4-8"), "{text:?}");
        assert!(!text.contains("knowledge cutoff"), "{text:?}");
        assert!(text.contains("\n - You are an interactive agent."), "{text:?}");
    }

    #[test]
    fn the_model_catalogue_goes_whole() {
        // The real line, verbatim: one bullet, ~330 characters, present tense
        // and plural ("models are") where the rule had only ever matched a
        // different phrasing. The catalogue therefore survived the prune.
        let mut body = json!({
            "system": concat!(
                " - The most recent Claude models are the Claude 5 family and Haiku 4.5. ",
                "Model IDs — Fable 5.1: 'claude-fable-5-1', Opus 5: 'claude-opus-5', ",
                "Sonnet 5: 'claude-sonnet-5', Haiku 4.5: 'claude-haiku-4-5-20251001'. ",
                "When building AI applications, default to the latest and most capable Claude models.\n",
                " - Kept.",
            ),
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("most recent Claude models"), "{text:?}");
        assert!(!text.contains("claude-fable-5-1"), "{text:?}");
        assert!(!text.contains("default to the latest and most capable"), "{text:?}");
        assert!(text.contains("- Kept."), "{text:?}");
    }

    #[test]
    fn the_bash_tool_description_carries_the_same_trailer_and_is_rewritten() {
        // The regression. The client puts its commit and PR conventions in the
        // `Bash` tool's description, not the system prompt, so a rewrite scoped
        // to `system` left both the attribution and the PR footer in place.
        let mut body = json!({
            "system": "An unrelated prompt.",
            "tools": [{
                "name": "Bash",
                "description": concat!(
                    "Runs a command.\n",
                    "- End git commit messages with:\n",
                    "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>\n",
                    "- End PR bodies with:\n",
                    "🤖 Generated with [Claude Code](https://claude.com/claude-code)",
                ),
            }],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["tools"][0]["description"].as_str().unwrap();
        assert!(
            text.contains("Co-authored-by: DeepSeek <noreply@deepseek.com>"),
            "{text:?}"
        );
        assert!(!text.contains("anthropic"), "{text:?}");
        assert!(!text.contains("Generated with"), "{text:?}");
        assert!(text.contains("Runs a command."), "{text:?}");
    }

    #[test]
    fn a_note_between_the_name_and_the_mail_does_not_hide_the_trailer() {
        // Every bracket shape a client has shipped, and one line that is prose
        // rather than a trailer. The `(1M context)` shape is why this is a scan
        // and not a single `find`: the first bracket pair wraps a note, so a
        // matcher that stops at the first pair finds no address, reports no
        // trailer, and leaves `Co-Authored-By: Claude` in the body — the exact
        // failure this was meant to prevent.
        let cases = [
            (
                "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>",
                true,
            ),
            ("Co-Authored-By: Claude 4.8 <noreply@anthropic.com>", true),
            ("Co-Authored-By: Claude (noreply@anthropic.com)", true),
            ("Co-Authored-By: (see the docs)", false),
        ];
        for (line, rewritten) in cases {
            let mut body = json!({ "tools": [{ "name": "Bash", "description": line }] });
            apply(&mut body, Vendor::Deepseek, "deepseek-flash");
            let text = body["tools"][0]["description"].as_str().unwrap();
            if rewritten {
                assert_eq!(text, "Co-authored-by: DeepSeek <noreply@deepseek.com>", "{line:?}");
            } else {
                assert_eq!(text, line, "prose about the trailer was rewritten: {line:?}");
            }
        }
    }

    #[test]
    fn a_property_description_inside_a_tool_schema_is_rewritten() {
        // The prose is not only at the top of a tool: every property carries a
        // description of its own, written by the same client.
        let mut body = json!({
            "tools": [{
                "name": "Task",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "string",
                            "description": "End it with Co-Authored-By: Claude <noreply@anthropic.com>",
                        },
                    },
                },
            }],
        });
        apply(&mut body, Vendor::Openai, "gpt-5.6-luna");
        let text = body["tools"][0]["input_schema"]["properties"]["body"]["description"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("Co-authored-by: OpenAI gpt-5.6-luna <noreply@openai.com>"),
            "{text:?}"
        );
    }

    #[test]
    fn a_tool_name_and_its_enum_are_left_alone() {
        // Only `description` is prose. A rewrite that walked every string would
        // rewrite the names and the enum values the protocol dispatches on.
        let mut body = json!({
            "tools": [{
                "name": "Claude",
                "description": "d",
                "input_schema": { "enum": ["Claude Code", "Co-Authored-By: Claude <x@y>"] },
            }],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        assert_eq!(body["tools"][0]["name"], "Claude");
        assert_eq!(body["tools"][0]["input_schema"]["enum"][0], "Claude Code");
        assert_eq!(
            body["tools"][0]["input_schema"]["enum"][1],
            "Co-Authored-By: Claude <x@y>"
        );
    }

    #[test]
    fn the_security_paragraph_is_dropped_for_other_vendors_only() {
        let paragraph = concat!(
            "IMPORTANT: Assist with authorized security testing, defensive security, CTF challenges, ",
            "and educational contexts. Refuse requests for destructive techniques, DoS attacks, ",
            "mass targeting, supply chain compromise, or detection evasion for malicious purposes. ",
            "Dual-use security tools (C2 frameworks, credential testing, exploit development) ",
            "require clear authorization context: pentesting engagements, CTF competitions, ",
            "security research, or defensive use cases.",
        );
        let mut body = json!({ "system": format!("Preface.\n{paragraph}\nEpilogue.") });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("C2 frameworks"), "{text:?}");
        assert!(!text.contains("IMPORTANT"), "{text:?}");
        assert!(text.contains("Preface."), "{text:?}");
        assert!(text.contains("Epilogue."), "{text:?}");

        // Anthropic's own request keeps it: this is their text, addressed to
        // their model, and the rewrite is not in the business of editing it.
        let mut body = json!({ "system": paragraph });
        apply(&mut body, Vendor::Anthropic, "claude-opus-5");
        assert_eq!(body["system"].as_str().unwrap(), paragraph);
    }

    #[test]
    fn the_agent_rule_goes_when_no_agent_tool_is_left() {
        let rule = "Do not use the Agent tool, workflows, or deep-research unless the user, a \
                    CLAUDE.md file, or a skill asks for it.";
        let mut body = json!({
            "system": format!("{rule}\nKept."),
            "tools": [{ "name": "Bash", "description": "Run a command." }],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("Agent tool"), "{text:?}");
        assert!(text.contains("Kept."), "{text:?}");
    }

    #[test]
    fn the_agent_rule_stays_while_the_agent_tool_is_offered() {
        let rule = "Do not use the Agent tool, workflows, or deep-research unless the user, a \
                    CLAUDE.md file, or a skill asks for it.";
        let mut body = json!({
            "system": rule,
            "tools": [{ "name": "Agent", "description": "Spawn an agent." }],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        // The other name the same tool ships under.
        let mut body_b = json!({
            "system": rule,
            "tools": [{ "name": "Task", "description": "Spawn a subagent." }],
        });
        apply(&mut body_b, Vendor::Deepseek, "deepseek-flash");
        assert!(body["system"].as_str().unwrap().contains("Agent tool"));
        assert!(body_b["system"].as_str().unwrap().contains("Agent tool"));
    }

    #[test]
    fn the_agent_rule_is_kept_when_the_body_sends_no_tools() {
        // No `tools[]` is not evidence the tool was removed. Deleting an
        // instruction on a guess is the failure this guards against.
        let rule = "Do not use the Agent tool, workflows, or deep-research unless the user, a \
                    CLAUDE.md file, or a skill asks for it.";
        let mut body = json!({ "system": rule });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        assert!(body["system"].as_str().unwrap().contains("Agent tool"));
    }

    #[test]
    fn a_tool_schema_description_is_rewritten_like_the_system_prompt() {
        // The reported failure: the commit convention lives in the Bash tool's
        // description, and a `system`-only walk never reached it.
        let mut body = json!({
            "tools": [{
                "name": "Bash",
                "description": "Run a command. End git commit messages with:\n\
                                Co-Authored-By: Claude 4.8 (noreply@anthropic.com)\n\
                                - End PR bodies with:\n\
                                \u{1f916} Generated with [Claude Code](https://claude.com/claude-code)",
            }],
        });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["tools"][0]["description"].as_str().unwrap();
        assert!(!text.contains("Claude"), "{text:?}");
        assert!(!text.contains("claude.com"), "{text:?}");
        assert!(text.contains("Co-authored-by: DeepSeek"), "{text:?}");
    }

    #[test]
    fn the_security_paragraph_is_found_behind_a_prefix() {
        // The reason it is matched on the tail: a `starts_with` would need the
        // sentence to open its own line, and the client is free to mark it up.
        //
        // The paragraph is on a line of its own, as the client sends it. A
        // neighbour on the same line would go with it — `dropped` answers per
        // line, so anything sharing the line of a dropped rule is dropped too.
        let paragraph = concat!(
            "IMPORTANT: Assist with authorized security testing, defensive security, ",
            "CTF challenges, and educational contexts. Refuse requests for destructive ",
            "techniques. Dual-use security tools require clear authorization context: ",
            "pentesting engagements, CTF competitions, security research, or defensive ",
            "use cases.",
        );
        let mut body = json!({ "system": format!("1. {paragraph}\nThen continue.") });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        let text = body["system"].as_str().unwrap();
        assert!(!text.contains("CTF challenges"), "{text:?}");
        assert!(text.contains("Then continue."), "{text:?}");
    }

    #[test]
    fn an_instruction_that_only_opens_like_the_paragraph_is_kept() {
        // The other half of the trade-off. Matching the generic head would
        // delete this line; matching the unique tail cannot.
        let mine = "IMPORTANT: Assist with authorized security testing tools \
                    internally, for the red-team exercise next month.";
        let mut body = json!({ "system": mine });
        apply(&mut body, Vendor::Deepseek, "deepseek-flash");
        assert_eq!(body["system"].as_str().unwrap(), mine);
    }
}
