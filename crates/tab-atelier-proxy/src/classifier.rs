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
    /// **Nothing produces this, and nothing may select it.**
    ///
    /// Claude Code's plan mode is the read-only investigation phase, and it is
    /// the cheapest traffic in a session to redirect — nothing in it writes, so
    /// moving it changes a suggestion rather than an action. It is not routable
    /// today because it cannot be identified: the authoritative marker is
    /// `permissionMode`, client-side state that never reaches a request, and both
    /// plan tools are declared in *every* request regardless of mode (see
    /// [`kind_of`] and `docs/proxy-tools.md`).
    ///
    /// Two wrong attempts are recorded here so neither is retried. A body detector
    /// keyed on `ExitPlanMode` matched every request, which would have sent real
    /// work to the cheap model. A per-account declaration in its place made one
    /// person's *entire* traffic — edits, tool calls, everything — count as plan
    /// mode, which is the same mistake at a different level.
    ///
    /// The variant survives for one reason: a stored rule may already say
    /// `"kind": "plan"`, and `users::Store::load` refuses an unknown enum value,
    /// so removing it would stop the whole account file from loading on upgrade.
    /// Both entry points that could *create* such a rule now refuse it.
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

/// Which kind of traffic this request is.
///
/// ## Plan mode is deliberately NOT detected here
///
/// It was, via `ExitPlanMode` in the declared tools, and **that detector was
/// wrong**: `docs/proxy-tools.md` measures a real Claude Code request and lists
/// `EnterPlanMode` and `ExitPlanMode` among the 17 harness tools that are
/// declared in *every* request — 4,011 B of description for `EnterPlanMode`
/// alone. Tool definitions live in the stable prompt prefix for cache reasons, so
/// they cannot vary by permission mode; the harness enforces plan mode, it does
/// not remove tools from the list.
///
/// So the tool was present in every body, every request classified as plan mode,
/// and any rule written for plan traffic would have caught everything. That is
/// the dangerous direction: a false positive sends real, writing traffic to
/// whatever cheap model plan mode was pointed at.
///
/// There is no verified wire signal for plan mode. `permissionMode`, which is
/// authoritative, is client-side state that never reaches a request. Until a real
/// capture shows something better, plan mode is *declared per account* instead —
/// see `Account::kind_override` — which is honest about who decided.
#[must_use]
pub fn kind_of(body: &serde_json::Value) -> Kind {
    if is_classifier(body) {
        Kind::Classifier
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

    fn with_tools(names: &[&str]) -> serde_json::Value {
        json!({ "tools": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })
    }

    // --- plan mode is NOT detectable from the body ---------------------------
    //
    // These assert the invariant that disproved the first implementation, so it
    // cannot be reintroduced by someone reading only the kind's name.

    /// The plan tools are declared in EVERY request, so they are not a signal.
    ///
    /// `docs/proxy-tools.md` measures a real Claude Code body: `EnterPlanMode`
    /// (4,011 B of description) and `ExitPlanMode` are two of the 17 harness tools
    /// present regardless of permission mode — tool definitions sit in the stable
    /// prompt prefix, so they cannot vary by mode without breaking the cache, and
    /// the harness enforces plan mode rather than removing tools.
    ///
    /// An earlier `is_plan` keyed on `ExitPlanMode`. It matched every Claude Code
    /// request, so every request was classified as plan mode and any rule written
    /// for plan traffic would have caught all of it.
    #[test]
    fn the_plan_tools_are_not_a_plan_mode_signal() {
        let ordinary = with_tools(&["Bash", "Read", "Edit", "EnterPlanMode", "ExitPlanMode"]);

        assert_eq!(
            kind_of(&ordinary),
            Kind::Work,
            "a normal request declaring both plan tools is still ordinary work",
        );
    }

    /// signals is judged by the one that actually identifies it.
    #[test]
    fn the_classifier_is_identified_even_beside_the_plan_tools() {
        let both = json!({
            "stop_sequences": ["</severity>"],
            "system": [{ "type": "text", "text": "You are a security monitor." }],
            "tools": [{ "name": "EnterPlanMode" }, { "name": "ExitPlanMode" }],
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
