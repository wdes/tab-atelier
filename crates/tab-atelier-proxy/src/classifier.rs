// SPDX-License-Identifier: MPL-2.0

//! The auto-mode permission classifier.
//!
//! Claude Code's auto mode does not decide on its own whether a gated action
//! (`Bash`, `Edit`, …) may run. It asks. For each action it makes a SECOND,
//! separate Messages call whose `system` prompt is Anthropic's security-monitor
//! prompt, whose single user turn is the transcript plus a `cc_automode_permissions`
//! block, and whose output is one tag the client PARSES rather than reads:
//!
//! ```text
//! max_tokens: 64, stop_sequences: ["</severity>"], thinking: {"type":"disabled"}
//! ```
//!
//! None of that is visible in the transcript, so nothing here used to know the
//! class existed. It routed as ordinary work, which means four decisions were
//! being taken implicitly:
//!
//! 1. A money-saving mapping (`opus → deepseek-flash`) also chose who
//!    adjudicates `rm -rf`. A mapping is a statement about WORK; the classifier
//!    is the gate that decides whether work runs at all.
//! 2. The classifier carries Anthropic's own monitor prompt, relayed verbatim
//!    to whichever third party the mapping named, once per gated action.
//! 3. The tool policy in `docs/proxy-tools.md` would have injected a `tools[]`
//!    into a body that has none — handing the judge a toolkit it was never
//!    written to hold.
//! 4. Call counts and token averages counted the judge as work, so one gated
//!    action read as two calls and the per-account figures answered a question
//!    nobody asked.
//!
//! So the class is detected here, exempted from (1) and (3) by construction,
//! exempted from compaction by `shape_body`, and labelled in inspection so the
//! judge can be told apart from the work it guards. It is NOT taken out of the
//! per-account totals — see the note below, and the panel's `auto-mode` badge
//! is what makes that readable rather than misleading.
//!
//! One live finding is worth recording, because it is invisible in the code
//! that has to preserve it: probing `DeepSeek`'s Anthropic-compatible endpoint
//! with this body, `deepseek-flash` WITHOUT `thinking:{"type":"disabled"}`
//! returns `stop_reason: max_tokens` and EMPTY content — a reasoning model that
//! spends all 64 tokens thinking and never emits a verdict. With the field it
//! answers in 9. Stripping, rewriting or defaulting `thinking` on this traffic
//! therefore turns every gated action into "auto mode cannot determine the
//! safety". Nothing in the proxy touches it, and the forwarding test asserts
//! that stays true.

use serde::{Deserialize, Serialize};

/// The tag the classifier's output is terminated by, and the one string that
/// identifies the request beyond doubt: it appears in `stop_sequences` only
/// because the caller means to parse the verdict.
const VERDICT_TAG: &str = "</severity>";

/// Opening words of Anthropic's security-monitor system prompt. The second
/// signal, for a caller that sends the classifier without the stop sequence.
const SYSTEM_MARKER: &str = "You are a security monitor for autonomous AI coding agents";

/// Which of the two kinds of traffic this is.
///
/// Deliberately narrow. It exists so that a mapping, a compaction level and a
/// tool policy — all statements about work — can be held to work alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ordinary model traffic: the conversation, and the tools run for it.
    #[default]
    Work,
    /// Claude Code's auto-mode permission classifier. Not work: a second,
    /// per-action call whose output is a parsed verdict rather than a reply.
    Classifier,
}

impl Kind {
    /// Whether the mapping table may retarget this call.
    ///
    /// False for the classifier. An operator rewriting `opus → deepseek-flash`
    /// to save money has not said "let deepseek-flash decide whether this is
    /// safe", and there is no way to read the second statement out of the
    /// first. A pin is still honoured — that is an explicit statement about
    /// destination — and the class ladder still runs, so the call lands on a
    /// cheap model either way.
    #[must_use]
    pub const fn retargets_by_mapping(self) -> bool {
        matches!(self, Self::Work)
    }

    /// Whether this may be compacted.
    ///
    /// False for the classifier. The transcript it judges is TEXT inside its
    /// single user turn, not `tool_result` blocks, so today's pass would find
    /// nothing to elide — but that is a coincidence of the current elision
    /// target, and the pass must never be the thing deciding which part of a
    /// safety judgement the judge gets to see.
    #[must_use]
    pub const fn compacts(self) -> bool {
        matches!(self, Self::Work)
    }

    /// Whether the tool policy in `docs/proxy-tools.md` may touch it.
    ///
    /// False for the classifier: that policy ADDS `tools[]` to a body that has
    /// none, and a judge written to emit one tag must not be handed a toolkit.
    #[must_use]
    pub const fn shapes_tools(self) -> bool {
        matches!(self, Self::Work)
    }
}

// Deliberately absent: an exemption from admission. The classifier looks tiny —
// `max_tokens: 64`, one tag out — but its INPUT is the whole transcript,
// re-sent once per gated action, so it spends the subscription for real.
// Exempting it would have been a budget hole dressed as a courtesy. It is
// admitted like anything else; what changes is that it is LABELLED, so the
// per-account figures can be read as work rather than inflated by the judge.

/// Whether this request body is the auto-mode permission classifier.
///
/// Two independent signals, either sufficient, both structural rather than
/// heuristic. The asymmetry is the point: a body that merely CONTAINS the
/// words "security monitor" in its transcript — someone asking why auto mode
/// is failing, say — is classified as work and costs nothing, while a missed
/// classifier is exempted from nothing and leaves exactly today's behaviour.
/// False negatives are the safe direction.
#[must_use]
pub fn is_classifier(body: &serde_json::Value) -> bool {
    // `</severity>` in `stop_sequences` means the caller means to parse the
    // output. No other caller of this API sets it.
    let verdict_terminated = body
        .get("stop_sequences")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|seqs| seqs.iter().any(|s| s.as_str() == Some(VERDICT_TAG)));
    verdict_terminated || mentions_monitor_prompt(body)
}

/// Whether any `system` text carries Anthropic's monitor prompt.
fn mentions_monitor_prompt(body: &serde_json::Value) -> bool {
    match body.get("system") {
        // A bare string prompt, or — what Claude Code actually sends — blocks.
        Some(serde_json::Value::String(s)) => s.contains(SYSTEM_MARKER),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains(SYSTEM_MARKER))
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The classifier as it actually arrives, trimmed to the fields detection
    /// reads. Reproduced from a captured body rather than imagined.
    fn classifier() -> serde_json::Value {
        json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 64,
            "stop_sequences": ["</severity>"],
            "thinking": {"type": "disabled"},
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "You are a security monitor for autonomous AI coding agents.\n\n\
                    <transcript>{{TRANSCRIPT}}</transcript>"},
            ],
            "messages": [{"role": "user", "content": "<cc_automode_permissions>Bash</cc_automode_permissions>"}],
        })
    }

    /// Ordinary traffic: a conversation with tools, no verdict to parse.
    fn work() -> serde_json::Value {
        json!({
            "model": "claude-opus-5",
            "max_tokens": 32000,
            "system": [{"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}],
            "messages": [{"role": "user", "content": "why is auto mode failing?"}],
            "tools": [{"name": "Bash", "description": "run a command"}],
        })
    }

    #[test]
    fn the_real_thing_is_recognised() {
        assert!(is_classifier(&classifier()));
    }

    #[test]
    fn ordinary_traffic_is_not() {
        assert!(!is_classifier(&work()));
    }

    /// The stop sequence alone is enough — a caller that sends the classifier
    /// with an empty or rewritten system prompt is still the classifier.
    #[test]
    fn the_verdict_tag_alone_identifies_it() {
        let mut body = work();
        body["stop_sequences"] = json!(["</severity>"]);
        assert!(is_classifier(&body));
    }

    /// A stop sequence that merely contains the tag as a substring is not the
    /// contract, and must not exempt traffic that has a real reply to give.
    #[test]
    fn a_lookalike_stop_sequence_does_not() {
        let mut body = work();
        body["stop_sequences"] = json!(["</severity> or something else"]);
        assert!(!is_classifier(&body));
    }

    /// A system prompt sent as a bare string, which the block form does not
    /// cover. Same signal, different encoding.
    #[test]
    fn a_string_system_prompt_is_checked_too() {
        let mut body = work();
        body["system"] = json!("You are a security monitor for autonomous AI coding agents.");
        assert!(is_classifier(&body));
    }

    /// A user ASKING about the monitor prompt is work — the marker in a message
    /// rather than in `system` proves nothing, which is exactly why only
    /// `system` is searched.
    #[test]
    fn a_conversation_about_the_classifier_is_work() {
        let mut body = work();
        body["messages"] = json!([{
            "role": "user",
            "content": "You are a security monitor for autonomous AI coding agents. Why is this failing?"
        }]);
        assert!(!is_classifier(&body));
    }

    #[test]
    fn malformed_bodies_are_work() {
        for body in [
            json!({}),
            json!([]),
            json!({"stop_sequences": "not-an-array"}),
            json!({"stop_sequences": [1, 2, 3]}),
            json!({"system": 42}),
            json!({"system": [{"text": null}]}),
            json!({"system": [{}]}),
        ] {
            assert!(!is_classifier(&body), "should not fire: {body}");
        }
    }

    #[test]
    fn only_work_is_retargeted_compacted_or_shaped() {
        assert!(Kind::Work.retargets_by_mapping());
        assert!(Kind::Work.compacts());
        assert!(Kind::Work.shapes_tools());
        assert!(!Kind::Classifier.retargets_by_mapping());
        assert!(!Kind::Classifier.compacts());
        assert!(!Kind::Classifier.shapes_tools());
    }
    /// Existing `inspect.jsonl` has no `kind`, and a file that fails to parse
    /// costs the whole history. `Work` is the right assumption for it.
    #[test]
    fn a_capture_without_a_kind_reads_as_work() {
        assert_eq!(Kind::default(), Kind::Work);
        assert_eq!(
            serde_json::from_str::<Kind>("\"classifier\"").unwrap(),
            Kind::Classifier
        );
        assert_eq!(serde_json::to_string(&Kind::Classifier).unwrap(), "\"classifier\"");
    }
}
