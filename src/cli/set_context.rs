// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-context "<text>" [--tab <id>] [--clear]`
//!
//! Lets an in-tab agent (Claude, a shell hook, …) declare what it's
//! working on — a PR, an issue, a task. The text is stored on the tab
//! and shown as a hover tooltip on the GUI tab name, plus surfaced on
//! `/tabs`, so a glance at the tab bar tells you what each agent is up
//! to.
//!
//! Defaults to the caller's own tab (`_TAB_ID`, injected into every
//! PTY); `--tab <id>` targets another tab (e.g. an orchestrator
//! labelling a worker it spawned). Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env — same as
//! `set-status`.

use std::time::Duration;

/// Parse `[--tab <id>] [--clear] <text…>` and POST it to the tab's
/// `/context` endpoint.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut clear = false;
    let mut tab_override: Option<String> = None;
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clear" => clear = true,
            "--tab" => {
                i += 1;
                let Some(t) = args.get(i) else {
                    eprintln!("set-context: --tab expects a tab id");
                    return 2;
                };
                tab_override = Some(t.clone());
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-context [--tab <id>] \"<text>\"  |  --clear\n\
                     Declare what this tab is working on (PR/issue/task). Shows as a hover\n\
                     tooltip on the GUI tab name and on /tabs. Defaults to the current tab.\n\
                     Examples:\n  \
                       tab-atelier set-context \"PR #3719: dompdf font reproduction\"\n  \
                       tab-atelier set-context --clear"
                );
                return 0;
            }
            other if !other.starts_with("--") => parts.push(other.to_string()),
            other => {
                eprintln!("set-context: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // Outside a tab-atelier tab the API env isn't exported. Treat that
    // as a silent no-op (exit 0) — exactly like `set-status` — so a
    // UserPromptSubmit / SessionEnd hook wired to this can never block
    // prompt submission or spam errors when `claude` runs outside any
    // tab. Once the env IS present we surface real failures normally.
    let (Ok(api_url), Ok(api_token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) else {
        return 0;
    };

    let context: Option<String> = if clear {
        None
    } else {
        let s = parts.join(" ");
        if s.trim().is_empty() { None } else { Some(s) }
    };
    if context.is_none() && !clear {
        eprintln!("set-context: nothing to set — pass text, or --clear (see --help)");
        return 2;
    }

    let tab_id = match tab_override.or_else(|| std::env::var("_TAB_ID").ok()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            eprintln!("set-context: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
            return 1;
        }
    };

    let cleared = context.is_none();
    let body = serde_json::json!({ "context": context }).to_string();
    let url = format!("{api_url}/tabs/by-id/{tab_id}/context");
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent();
    match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .send(&body)
    {
        Ok(_) => {
            if cleared {
                println!("✓ tab context cleared");
            } else {
                println!("✓ tab context set");
            }
            0
        }
        Err(e) => {
            eprintln!("set-context: {e}");
            1
        }
    }
}
