// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-assignment "[<project>:]<phase>/<role>" [--tab <id>] [--clear]`
//!
//! Declares an agent's **stable place in the workflow** — its phase and
//! role — for the harness dashboard. Unlike `set-context` (rewritten by
//! the prompt hook every turn and never persisted), `assignment` is set
//! **once**, immune to the hook, and persisted to `tabs.json`, so it's
//! the source of truth the dashboard maps a tab onto a phase node + a
//! project from.
//!
//! `<phase>` ∈ {scope, plan, build, review, verify, sweep, done};
//! `<role>` is a free label (implementer, reviewer, orchestrator…); the
//! optional `<project>:` prefix overrides the project derived from cwd.
//!
//! Mirrors `set-context`: defaults to the caller's own tab (`_TAB_ID`),
//! `--tab <id>` targets another, `--clear` removes it. Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env.

use std::time::Duration;

/// Parse `[--tab <id>] [--clear] <assignment…>` and POST it to the tab's
/// `/assignment` endpoint.
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
                    eprintln!("set-assignment: --tab expects a tab id");
                    return 2;
                };
                tab_override = Some(t.clone());
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-assignment [--tab <id>] \"[<project>:]<phase>/<role>\"  |  --clear\n\
                     Declare this tab's stable workflow place (phase + role) for the dashboard.\n\
                     Unlike set-context (the volatile prompt label), assignment is set once,\n\
                     hook-immune, and persisted. Defaults to the current tab.\n\
                     Examples:\n  \
                       tab-atelier set-assignment \"build/implementer\"\n  \
                       tab-atelier set-assignment \"kalpin-back:review/reviewer\"\n  \
                       tab-atelier set-assignment --clear"
                );
                return 0;
            }
            other if !other.starts_with("--") => parts.push(other.to_string()),
            other => {
                eprintln!("set-assignment: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // Outside a tab-atelier tab the API env isn't exported — silent no-op
    // (exit 0), exactly like set-context / set-status.
    let (Ok(api_url), Ok(api_token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) else {
        return 0;
    };

    let assignment: Option<String> = if clear {
        None
    } else {
        let s = parts.join(" ");
        if s.trim().is_empty() { None } else { Some(s) }
    };
    if assignment.is_none() && !clear {
        eprintln!("set-assignment: nothing to set — pass an assignment, or --clear (see --help)");
        return 2;
    }

    let tab_id = match tab_override.or_else(|| std::env::var("_TAB_ID").ok()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            eprintln!("set-assignment: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
            return 1;
        }
    };

    let cleared = assignment.is_none();
    let body = serde_json::json!({ "assignment": assignment }).to_string();
    let url = format!("{api_url}/tabs/by-id/{tab_id}/assignment");
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
                println!("✓ tab assignment cleared");
            } else {
                println!("✓ tab assignment set");
            }
            0
        }
        Err(e) => {
            eprintln!("set-assignment: {e}");
            1
        }
    }
}
