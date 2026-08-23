// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
//! Events handled:
//! - `session-start`  → state=thinking, kind=claude, sessionId=`<id>`
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

/// True when a prompt is an AUTOMATED tick (watcher/cron/round/sage wakeup/roster
/// refresh/restart) typed AS a prompt, not a genuine human direction (S9, mirror
/// of the activity-scribe `CRON_TICK` detection). Matches known tick PHRASES as a
/// PREFIX of the trimmed prompt — a real prompt starts with an action ("fix …",
/// "PR #…"), so "fix the watcher restart bug" is NOT a tick while "Watcher
/// restart after OOM" is.
fn is_cron_tick(prompt: &str) -> bool {
    const TICK_PREFIXES: &[&str] = &[
        "restart mx",
        "ronde ",
        "watcher restart",
        "réveil ",
        "maintien de la session",
        "refresh orchestrateur",
    ];
    let p = prompt.trim().to_lowercase();
    TICK_PREFIXES.iter().any(|t| p.starts_with(t))
}

/// True when a prompt is a GENUINE human direction worth mirroring onto the
/// `direction` blackboard (S9): non-empty, not a synthetic injection, not a
/// nudge, not a `--flag`, and not an automated cron/watcher tick.
fn is_human_direction(prompt: &str) -> bool {
    let p = prompt.trim();
    !p.is_empty() && !is_synthetic_prompt(p) && !is_nudge(p) && !p.starts_with("--") && !is_cron_tick(p)
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let Some(event) = args.first().map(String::as_str) else {
        eprintln!("usage: tab-atelier-headless claude-hook <event>");
        eprintln!("  events: session-start, user-prompt, pre-tool, post-tool, stop, notification, session-end");
        return 2;
    };

    // Slurp the hook event JSON from stdin. Tiny — a few KB at most
    // for tool inputs.
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    let payload: serde_json::Value = serde_json::from_str(&stdin_buf).unwrap_or(serde_json::Value::Null);
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
                // S9: mirror a GENUINE human direction onto the `direction`
                // blackboard (like the dispatch mirror), so the fleet knows where
                // the PO stands — but NEVER a cron/watcher tick, nudge, synthetic
                // injection, or flag (is_human_direction gates all of those).
                // Best-effort; `from` = this tab's `_TAB_ID` when in a tab.
                if is_human_direction(p) {
                    let snippet: String = p.chars().take(200).collect();
                    let from = std::env::var("_TAB_ID").ok().filter(|s| !s.is_empty());
                    crate::cli::team::note_best_effort(Some("direction".to_string()), from, &snippet);
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
    use super::{is_cron_tick, is_human_direction, is_nudge, is_synthetic_prompt};

    // --- Increment 6 S9 (REFINER red) — mirror REAL human directions onto the
    //     `direction` blackboard topic (like the dispatch mirror), so the fleet
    //     knows where the PO stands. The user-prompt hook, AFTER stamping the
    //     context, posts to topic `direction` IFF the prompt is a genuine human
    //     direction — NEVER a cron/watcher tick (reusing S7's detection), a nudge,
    //     or a synthetic injection. RED until is_cron_tick / is_human_direction
    //     exist. Builder: rust (S9-hook).
    #[test]
    fn cron_ticks_are_detected() {
        // Automated ticks that arrive typed AS prompts (watchers, rounds, sage
        // wakeups, roster refresh) — mirror of scripts/activity-scribe CRON_TICK.
        assert!(is_cron_tick("RESTART mx imminent — sauvegarde en cours"));
        assert!(is_cron_tick("Ronde du matin : check the fleet"));
        assert!(is_cron_tick("Watcher restart after OOM"));
        assert!(is_cron_tick("réveil du sage"));
        assert!(is_cron_tick("maintien de la session"));
        assert!(is_cron_tick("refresh orchestrateurs"));
        // A real prompt that merely mentions a watcher is not a tick.
        assert!(!is_cron_tick("fix the watcher restart bug in api.rs"));
        assert!(!is_cron_tick("PR #42: refactor the parser"));
    }

    #[test]
    fn only_genuine_human_directions_are_mirrored() {
        // Real PO directions -> mirrored to `direction`.
        assert!(is_human_direction("PR #42: refactor the parser"));
        assert!(is_human_direction("go build increment 6, start with S7"));
        // Cron/watcher ticks (typed but automated) -> NOT mirrored.
        assert!(!is_human_direction("RESTART mx imminent — sauvegarde"));
        assert!(!is_human_direction("Ronde du matin : check the fleet"));
        // Nudges, synthetic injections, empty, and flag-like -> NOT mirrored.
        assert!(!is_human_direction("continue"));
        assert!(!is_human_direction("<task-notification>x</task-notification>"));
        assert!(!is_human_direction("   "));
        assert!(!is_human_direction("--clear"));
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
}
