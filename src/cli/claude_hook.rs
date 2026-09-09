// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier-headless claude-hook <event>`
//!
//! Bridge between Claude Code's hook system and tab-atelier's
//! `set-status` API. Reads the hook event JSON from stdin, extracts
//! `session_id` (and any state cues), and POSTs the matching state
//! to `/tabs/by-id/{tab_id}/status` via the same env-driven path
//! `set-status` uses (`_TAB_ID`, `TAB_ATELIER_API_URL`,
//! `TAB_ATELIER_API_TOKEN`).
//!
//! The system-wide hook config at
//! `/etc/claude-code/managed-settings.json` points every `claude`
//! invocation on the box at these subcommands, so the desktop LED /
//! tab badge tracks Claude Code state automatically without each
//! user having to wire their own settings.json.
//!
//! `session-start` also prints a `SessionStart` hook payload carrying
//! [`agent_brief`] as `additionalContext`, so every Claude that starts *inside
//! a tab* is told the coordination verbs exist without any per-user setup. It
//! is skipped outside a tab (no `_TAB_ID`) and with `TAB_ATELIER_NO_BRIEF=1`.
//!
//! Events handled:
//! - `session-start`  → state=thinking, kind=claude, sessionId=`<id>`, brief
//! - `user-prompt`    → state=thinking, and sets the tab context label
//!   to the submitted prompt (the tab name's hover tooltip then shows
//!   what the agent is working on)
//! - `pre-tool`       → state=thinking, label=`<tool_name>`
//! - `post-tool`      → state=thinking (no label — back to base)
//! - `stop`           → state=waiting (Claude finished a turn)
//! - `notification`   → state=waiting, label=`<message>`
//! - `session-end`    → state=idle, label=__clear__ (drops the
//!   persistent agent attachment so the LED actually goes dark;
//!   mirrored from set-status idle semantics) and clears the tab
//!   context label
//!
//! Failures are intentionally swallowed (exit 0) so a misconfigured
//! hook can never block Claude. Stderr gets a one-line note for
//! debugging.

use std::io::Read;

/// The text every agent is told when its session starts inside a tab.
///
/// Embedded rather than read from `/usr/share`, so a running instance can't be
/// left briefing agents from a file an upgrade moved. The markdown around the
/// markers is documentation for humans; only the marked span is injected.
const AGENT_BRIEF_DOC: &str = include_str!("../../docs/agent-brief.md");

/// Pull the injected span out of the doc.
///
/// Everything outside the markers explains the mechanism to whoever edits it —
/// useful to a human, pure cost to an agent that gets it on every session.
#[must_use]
pub fn extract_brief(doc: &str) -> &str {
    doc.split_once("<!-- BRIEF-START -->")
        .and_then(|(_, rest)| rest.split_once("<!-- BRIEF-END -->"))
        .map_or(doc, |(brief, _)| brief.trim())
}

/// Where an operator can override the built-in text.
fn brief_override_paths() -> Vec<std::path::PathBuf> {
    vec![
        crate::platform::config_dir().join("agent-brief.md"),
        std::path::PathBuf::from("/etc/tab-atelier/agent-brief.md"),
    ]
}

/// The brief to inject: the first override that exists, else the built-in.
#[must_use]
pub fn agent_brief() -> String {
    for p in brief_override_paths() {
        if let Ok(body) = std::fs::read_to_string(&p) {
            let trimmed = extract_brief(&body).trim().to_owned();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    extract_brief(AGENT_BRIEF_DOC).to_owned()
}

/// The `SessionStart` payload that puts `brief` in the model's context.
///
/// Claude Code reads this from the hook's stdout; anything else there is
/// ignored, so it is safe to print unconditionally as long as the JSON is
/// well-formed.
#[must_use]
pub fn brief_json(brief: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": brief,
        }
    })
    .to_string()
}

/// Whether this session should be briefed.
///
/// Only inside a tab: a Claude started from an ordinary terminal cannot use
/// any of these verbs, and telling it about siblings it doesn't have is worse
/// than silence. `TAB_ATELIER_NO_BRIEF=1` opts out entirely.
#[must_use]
pub fn should_brief(tab_id: Option<&str>, opt_out: Option<&str>) -> bool {
    if opt_out.is_some_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    tab_id.is_some_and(|id| !id.trim().is_empty())
}

/// True when a `UserPromptSubmit` payload is a system/tool injection
/// rather than a human-typed prompt — a background `<task-notification>`,
/// a `<system-reminder>`, a slash-command or `!`-bash expansion, etc.
/// These fire `UserPromptSubmit` with the wrapped XML block as `.prompt`,
/// and we don't want them overwriting the tab's context label with noise.
/// Heuristic: a genuine prompt almost never opens with a literal `<tag>`.
fn is_synthetic_prompt(prompt: &str) -> bool {
    let mut chars = prompt.trim_start().chars();
    chars.next() == Some('<') && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// True for bare resume/affirmation nudges that shouldn't replace a
/// tab's context label. "continue" is what the ⛑ brain auto-injects to
/// unstick an agent; the rest are common one-word "keep going" replies.
fn is_nudge(prompt: &str) -> bool {
    const NUDGES: &[&str] = &["continue", "go", "go on", "keep going", "proceed", "resume", "next"];
    let p = prompt.trim();
    NUDGES.iter().any(|n| p.eq_ignore_ascii_case(n))
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    // Slurp the hook event JSON from stdin. Tiny — a few KB at most
    // for tool inputs.
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    run_with_payload(args, &stdin_buf)
}

/// [`run`], with the hook payload already read.
///
/// Split out so the event handling can be tested: the payload is the only
/// input, and reading it from the process's stdin makes every branch here
/// unreachable from a test.
#[must_use]
pub fn run_with_payload(args: &[String], stdin_buf: &str) -> i32 {
    let Some(event) = args.first().map(String::as_str) else {
        eprintln!("usage: tab-atelier-headless claude-hook <event>");
        eprintln!("  events: session-start, user-prompt, pre-tool, post-tool, stop, notification, session-end");
        return 2;
    };

    let payload: serde_json::Value = serde_json::from_str(stdin_buf).unwrap_or(serde_json::Value::Null);
    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let tool_name = payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let notification = payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    // Context side-channel: keep the tab's hover label in sync with the
    // agent's work. `user-prompt` stamps the submitted prompt;
    // `session-end` clears it. set_context::run is a silent no-op when
    // the tab env isn't present, so this never blocks Claude.
    match event {
        "user-prompt" => {
            if let Some(p) = payload.get("prompt").and_then(serde_json::Value::as_str) {
                let p = p.trim();
                // Skip system/tool injections (background <task-notification>,
                // <system-reminder>, slash-command / !-bash expansions) — they
                // arrive as UserPromptSubmit with the wrapped XML as `.prompt`
                // and would clobber the label with noise, not the actual task.
                // Also skip a leading `--` so a prompt isn't mis-parsed as a
                // set-context flag, and bare nudges like "continue" (what the
                // brain auto-sends to a stuck agent, or you type to resume) —
                // those shouldn't overwrite the real PR/task context.
                if !p.is_empty() && !is_synthetic_prompt(p) && !p.starts_with("--") && !is_nudge(p) {
                    // Trim to a tooltip-sized snippet; the API caps at 2000
                    // chars anyway, but a one-line label reads better.
                    let snippet: String = p.chars().take(200).collect();
                    let _ = crate::cli::set_context::run(&[snippet]);
                }
            }
        }
        "session-end" => {
            let _ = crate::cli::set_context::run(&["--clear".to_owned()]);
        }
        _ => {}
    }

    // Map event → (state, label override). For SessionStart we also
    // pass `--kind claude --session <id>` so the daemon stamps the
    // durable attachment that drives auto-resume.
    if event == "session-start"
        && should_brief(
            std::env::var("_TAB_ID").ok().as_deref(),
            std::env::var("TAB_ATELIER_NO_BRIEF").ok().as_deref(),
        )
    {
        // Project briefs come after the built-in one: they are the specific
        // word, and specifics belong last.
        let mut brief = agent_brief();
        if let Ok(cwd) = std::env::current_dir() {
            let project = crate::briefs::for_cwd(&cwd);
            if !project.is_empty() {
                brief.push_str("\n\n");
                brief.push_str(&project);
            }
        }
        // Printed before the status POST so a slow or unreachable daemon can't
        // cost the session its context.
        println!("{}", brief_json(&brief));
    }

    let (state, label, with_attachment) = match event {
        "session-start" => ("thinking", None, true),
        "pre-tool" => ("thinking", tool_name, false),
        // user-prompt and post-tool both return to the base thinking
        // state with no label (user-prompt's context side-effect ran above).
        "user-prompt" | "post-tool" => ("thinking", None, false),
        "stop" => ("waiting", None, false),
        "notification" => ("waiting", notification, false),
        "session-end" => ("idle", Some("__clear__".to_owned()), false),
        other => {
            eprintln!("claude-hook: unknown event {other:?}");
            return 0;
        }
    };

    // Build the set-status arg vector and reuse the existing runner
    // so the env-discovery + 2s timeout + body shape are identical
    // to a manual `tab-atelier-headless set-status` call.
    let mut argv: Vec<String> = vec![state.into()];
    if let Some(l) = label {
        argv.push("--label".into());
        argv.push(l);
    }
    if with_attachment {
        argv.push("--kind".into());
        argv.push("claude".into());
        if let Some(sid) = session_id {
            argv.push("--session".into());
            argv.push(sid);
        }
    }
    let code = crate::cli::set_status::run(&argv);
    // Never propagate a failure to Claude Code — a hook that exits
    // non-zero can block tool execution. We've already logged
    // anything useful to stderr inside set_status::run.
    let _ = code;
    0
}

#[cfg(test)]
mod tests {
    use super::{agent_brief, brief_json, extract_brief, is_nudge, is_synthetic_prompt, should_brief};

    #[test]
    fn only_the_marked_span_reaches_the_model() {
        let doc = "# Title\n\nhuman notes\n<!-- BRIEF-START -->\nthe brief\n<!-- BRIEF-END -->\nmore notes\n";
        assert_eq!(extract_brief(doc), "the brief");
        // The surrounding prose explains the mechanism to whoever edits the
        // file; injecting it would be pure cost on every session.
        assert!(!extract_brief(doc).contains("human notes"));
        assert!(!extract_brief(doc).contains("more notes"));
        // A file without markers is used whole rather than dropped — an
        // operator writing a plain override should not get silence.
        assert_eq!(extract_brief("just text"), "just text");
    }

    #[test]
    fn the_shipped_brief_is_short_and_names_the_verbs() {
        let brief = agent_brief();
        // It is injected into every session on every tab, forever, so length
        // is a real cost and worth asserting rather than hoping about.
        assert!(
            brief.len() < 2_000,
            "the brief costs tokens in every session; {} chars is too long",
            brief.len()
        );
        for verb in ["peers", "dispatch", "take", "done", "wait"] {
            assert!(brief.contains(verb), "brief never mentions {verb}");
        }
        // The one rule that keeps two agents off one task.
        assert!(brief.contains("lease"), "the brief must explain why not to skip `take`");
        assert!(!brief.contains("BRIEF-START"), "markers leaked into the brief");
    }

    #[test]
    fn the_payload_is_the_shape_claude_code_reads() {
        let json: serde_json::Value = serde_json::from_str(&brief_json("hello")).unwrap();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(json["hookSpecificOutput"]["additionalContext"], "hello");
        // Newlines and quotes in the brief must not produce invalid JSON on
        // stdout — a malformed payload would be silently dropped.
        let awkward = "line1\n\"quoted\"\n\ttabbed";
        let round: serde_json::Value = serde_json::from_str(&brief_json(awkward)).unwrap();
        assert_eq!(round["hookSpecificOutput"]["additionalContext"], awkward);
    }

    #[test]
    fn only_sessions_inside_a_tab_are_briefed() {
        assert!(should_brief(Some("tab-abc"), None));
        // A Claude in an ordinary terminal can't use any of these verbs;
        // telling it about siblings it doesn't have is worse than silence.
        assert!(!should_brief(None, None));
        assert!(!should_brief(Some("  "), None));
        // And an explicit opt-out wins even inside a tab.
        assert!(!should_brief(Some("tab-abc"), Some("1")));
        assert!(should_brief(Some("tab-abc"), Some("0")), "0 means don't opt out");
        assert!(should_brief(Some("tab-abc"), Some("")));
    }

    #[test]
    fn nudges_do_not_overwrite_context() {
        // The brain auto-injects "continue"; these one-word resumes must
        // NOT replace a tab's real PR/task context.
        assert!(is_nudge("continue"));
        assert!(is_nudge("  Continue  "));
        assert!(is_nudge("CONTINUE"));
        assert!(is_nudge("go on"));
        assert!(is_nudge("keep going"));
        // Real prompts are kept.
        assert!(!is_nudge("continue the dompdf refactor"));
        assert!(!is_nudge("PR #42"));
        assert!(!is_nudge("continuent"));
    }

    #[test]
    fn synthetic_prompts_are_skipped() {
        // System / tool injections that arrive as UserPromptSubmit.
        assert!(is_synthetic_prompt("<task-notification>\n<task-id>b6co42m2k</task-id>"));
        assert!(is_synthetic_prompt("<system-reminder>do the thing</system-reminder>"));
        assert!(is_synthetic_prompt("  <command-name>/foo</command-name>"));
    }

    #[test]
    fn real_prompts_are_kept() {
        assert!(!is_synthetic_prompt("PR #3719: dompdf font reproduction"));
        assert!(!is_synthetic_prompt("continue"));
        // A bare comparison / math expression isn't a tag.
        assert!(!is_synthetic_prompt("< 5 items left"));
        assert!(!is_synthetic_prompt("<"));
    }

    fn hargs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn every_hook_event_is_handled_and_none_can_fail_the_session() {
        // The contract that matters most: a hook must NEVER exit non-zero,
        // because Claude Code treats that as a reason to block the tool call.
        // The daemon is unreachable in a test, which is exactly the failure
        // mode this has to swallow.
        let payload = r#"{"session_id":"abc-123","tool_name":"Bash","message":"waiting"}"#;
        for event in [
            "session-start",
            "user-prompt",
            "pre-tool",
            "post-tool",
            "stop",
            "notification",
            "session-end",
        ] {
            assert_eq!(
                super::run_with_payload(&hargs(&[event]), payload),
                0,
                "{event} must exit 0 even when the daemon is unreachable"
            );
        }
        // An unknown event is reported but still exits 0 — a future Claude
        // Code release adding an event must not start blocking tools.
        assert_eq!(super::run_with_payload(&hargs(&["not-an-event"]), payload), 0);
        // No event at all IS a usage error: that is a broken settings.json,
        // caught before it is wired into every session on the machine.
        assert_eq!(super::run_with_payload(&hargs(&[]), payload), 2);
    }

    #[test]
    fn a_malformed_payload_does_not_take_the_session_down() {
        // Whatever arrives on stdin, the hook exits 0. Claude Code has
        // changed this shape before, and a parse error must not become a
        // blocked tool call.
        for body in ["", "not json", "null", "[]", r#"{"session_id":null}"#, "{\"a\":"] {
            assert_eq!(
                super::run_with_payload(&hargs(&["pre-tool"]), body),
                0,
                "payload {body:?} broke the hook"
            );
        }
    }
}
