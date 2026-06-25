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
                // Trim to a tooltip-sized snippet; the API caps at 2000
                // chars anyway, but a one-line label reads better.
                let snippet: String = p.trim().chars().take(200).collect();
                if !snippet.is_empty() {
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
