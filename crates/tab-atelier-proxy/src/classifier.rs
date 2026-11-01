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
use tab_atelier_api::schemars;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, tab_atelier_api::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ordinary model traffic: the conversation, and the tools run for it.
    #[default]
    Work,
    /// Claude Code's auto-mode permission classifier. Not work: a second,
    /// per-action call whose output is a parsed verdict rather than a reply.
    ///
    /// `auto` is accepted as a spelling of this in configuration, because that
    /// is what the feature is called in Claude Code's own UI and in the
    /// conversation that asked for routing it. A rule an operator writes with
    /// the name they know is a rule that gets used.
    #[serde(alias = "auto", alias = "auto_mode")]
    Classifier,
    /// Claude Code's plan mode: the read-only investigation phase, before the
    /// agent is allowed to act.
    ///
    /// Not work in the sense the other rules care about, and not the classifier
    /// either. It is separated out because it is the cheapest possible traffic to
    /// redirect: nothing in it writes, so an operator can send it to a small model
    /// without changing what the agent produces — and plan mode is often the
    /// largest share of a session's thinking.
    ///
    /// Detected from the declared tools (see [`is_plan`]), which is a heuristic
    /// and is documented as one there. A missed plan call is routed as ordinary
    /// work, which is exactly today's behaviour; a false positive would send real
    /// work to the cheap model, so the detector is deliberately narrow.
    Plan,
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
    ///
    /// **True for plan mode.** A plan is an investigation: nothing in it writes,
    /// so moving it changes the quality of a suggestion rather than the safety of
    /// an action. That is exactly the traffic a cost rule is looking for.
    #[must_use]
    pub const fn retargets_by_mapping(self) -> bool {
        matches!(self, Self::Work | Self::Plan)
    }

    /// Whether this may be compacted.
    ///
    /// False for the classifier. The transcript it judges is TEXT inside its
    /// single user turn, not `tool_result` blocks, so today's pass would find
    /// nothing to elide — but that is a coincidence of the current elision
    /// target, and the pass must never be the thing deciding which part of a
    /// safety judgement the judge gets to see.
    ///
    /// True for plan mode: an investigation accumulates exactly the long tool
    /// output this exists to elide, so it is the case compaction helps most.
    #[must_use]
    pub const fn compacts(self) -> bool {
        matches!(self, Self::Work | Self::Plan)
    }

    /// Whether the tool policy in `docs/proxy-tools.md` may touch it.
    ///
    /// False for the classifier: that policy ADDS `tools[]` to a body that has
    /// none, and a judge written to emit one tag must not be handed a toolkit.
    ///
    /// **Also false for plan mode, and that one is not about the toolkit.** Plan
    /// mode is a promise that the agent will not act; the policy exists to hand a
    /// body a set of tools it did not ask for. Doing that inside the phase whose
    /// whole meaning is "no side effects" would break the promise the user was
    /// given — with tools this proxy chose, not ones the harness allowed.
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

/// Name of the tool Claude Code declares while plan mode is active.
///
/// `ExitPlanMode` is how the agent leaves the phase, so it is declared when — and
/// only when — there is a plan mode to leave. `EnterPlanMode` is *not* used as a
/// signal: it is the tool for asking to enter, and is available outside plan mode
/// by construction.
const PLAN_TOOL: &str = "ExitPlanMode";

/// Whether this request is Claude Code in plan mode.
///
/// ## This is a heuristic, and deliberately a narrow one
///
/// The authoritative marker is `permissionMode`, which is client-side state and
/// never appears in a request body. What the wire carries is the tool list, and
/// `ExitPlanMode` appears in it exactly while the phase is active.
///
/// The two failure directions are not symmetric, which is what decides the
/// narrowness: a **missed** plan call is routed as ordinary work — precisely
/// today's behaviour, nothing regresses — while a **false positive** would send
/// real, writing traffic to whatever cheap model plan mode was pointed at. So
/// this matches a declared tool by exact name and nothing else: no system-prompt
/// substring, no guessing from message content. If a real capture later shows a
/// stronger and stable marker, it belongs here, behind the same test.
#[must_use]
pub fn is_plan(body: &serde_json::Value) -> bool {
    body.get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name == PLAN_TOOL)
            })
        })
}

/// Which kind of traffic this request is.
///
/// Ordered so the positive identifications win: the classifier is recognised by
/// an unmistakable string that no other caller emits, and is checked first
/// because a classifier body is minimal and would not otherwise be mistaken for
/// plan mode — but the ordering makes that explicit rather than incidental.
#[must_use]
pub fn kind_of(body: &serde_json::Value) -> Kind {
    if is_classifier(body) {
        Kind::Classifier
    } else if is_plan(body) {
        Kind::Plan
    } else {
        Kind::Work
    }
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

    // --- plan mode ---------------------------------------------------------
    //
    // The authoritative marker is `permissionMode`, which never reaches the wire.
    // What the wire carries is the tool list, so these tests pin down exactly
    // which tool is the signal and — more importantly — which one is not.

    fn with_tools(names: &[&str]) -> serde_json::Value {
        json!({ "tools": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })
    }

    /// Plan mode is recognised by the tool that leaves it.
    #[test]
    fn plan_mode_is_detected_from_the_declared_tool() {
        assert!(is_plan(&with_tools(&["Bash", "Read", "ExitPlanMode"])));
    }

    /// `EnterPlanMode` is NOT the signal.
    ///
    /// It is the tool for *asking* to enter the phase, so it is declared outside
    /// plan mode by construction — treating it as the marker would classify every
    /// ordinary request as plan mode, and a false positive sends real work to
    /// whatever cheap model plan mode was pointed at. That is the asymmetric
    /// failure this detector is narrow to avoid.
    #[test]
    fn enter_plan_mode_is_not_the_signal() {
        assert!(!is_plan(&with_tools(&["Bash", "EnterPlanMode"])));
    }

    /// A body with no tools at all is not plan mode, and does not crash.
    #[test]
    fn a_body_without_tools_is_not_plan_mode() {
        assert!(!is_plan(&json!({})));
        assert!(!is_plan(&json!({ "tools": [] })));
        assert!(!is_plan(&json!({ "tools": "not an array" })));
    }

    /// The classifier is identified first, so a body that somehow carried both
    /// signals is judged by the safer of the two.
    #[test]
    fn the_classifier_takes_precedence_over_plan_mode() {
        let both = json!({
            "stop_sequences": ["</severity>"],
            "system": [{ "type": "text", "text": "You are a security monitor." }],
            "tools": [{ "name": "ExitPlanMode" }],
        });

        assert_eq!(kind_of(&both), Kind::Classifier);
    }

    /// Ordinary traffic stays Work.
    #[test]
    fn ordinary_traffic_is_work() {
        assert_eq!(kind_of(&with_tools(&["Bash", "Read"])), Kind::Work);
    }

    /// Plan mode may be redirected and compacted, but never given tools.
    ///
    /// The first two are why routing it is useful: an investigation writes
    /// nothing, so moving it changes the quality of a suggestion rather than the
    /// safety of an action, and it accumulates the long tool output compaction
    /// exists to elide.
    ///
    /// The third is the opposite: plan mode is a promise that the agent will not
    /// act, and the tool policy hands a body tools it did not ask for. Doing that
    /// inside the phase whose meaning is "no side effects" would break the promise
    /// with tools this proxy chose.
    #[test]
    fn plan_mode_is_redirectable_and_compactable_but_never_shaped() {
        assert!(Kind::Plan.retargets_by_mapping());
        assert!(Kind::Plan.compacts());
        assert!(!Kind::Plan.shapes_tools());

        // And the classifier keeps every gate closed, unchanged by this.
        assert!(!Kind::Classifier.retargets_by_mapping());
        assert!(!Kind::Classifier.compacts());
        assert!(!Kind::Classifier.shapes_tools());
    }

    /// The classifier as it actually arrives, trimmed to the fields detection
    /// reads. Reproduced from a captured body rather than imagined.
    fn classifier() -> serde_json::Value {
        json!({
            "model": "claude-haiku-5-5",
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
            "model": "claude-opus-5-5",
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
