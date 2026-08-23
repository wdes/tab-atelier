// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-rehome-status <state> [--tab <id>] [--clear]`
//!
//! Marks a PREDECESSOR tab's progress through the re-home bidirectional-proof
//! loop (`rehome-tab.sh`). `<state>` is one of `handoff-written`,
//! `successor-ready`, `ack-sent`, `safe-to-close`. The script stamps the first
//! three at its steps (`--tab <old-uuid>`); the old agent posts `safe-to-close`
//! on itself when it replies REHOME ACK — the final proof from its own side,
//! which is what unlocks the GUI "close the predecessor" action.
//!
//! Mirrors `set-assignment`: defaults to the caller's own tab (`_TAB_ID`),
//! `--tab <id>` targets another, `--clear` removes it. Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env.

use std::time::Duration;

/// Parse `[--tab <id>] [--clear] <state>` and POST it to the tab's `/rehome`.
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
                    eprintln!("set-rehome-status: --tab expects a tab id");
                    return 2;
                };
                tab_override = Some(t.clone());
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-rehome-status <state> [--tab <id>]  |  --clear\n\
                     Mark a predecessor tab's re-home progress (rehome-tab.sh).\n\
                     States: handoff-written | successor-ready | ack-sent | safe-to-close\n\
                     safe-to-close unlocks the GUI 'close the predecessor' action.\n\
                     Examples:\n  \
                       tab-atelier set-rehome-status successor-ready --tab <old-uuid>\n  \
                       tab-atelier set-rehome-status safe-to-close   # the old agent, on its ACK"
                );
                return 0;
            }
            other if !other.starts_with("--") => parts.push(other.to_string()),
            other => {
                eprintln!("set-rehome-status: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // Outside a tab-atelier tab the API env isn't exported — silent no-op
    // (exit 0), like set-assignment / set-status.
    let (Ok(api_url), Ok(api_token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) else {
        return 0;
    };

    let status: Option<String> = if clear {
        None
    } else {
        let s = parts.join(" ");
        if s.trim().is_empty() {
            None
        } else {
            Some(s.trim().to_string())
        }
    };
    if status.is_none() && !clear {
        eprintln!("set-rehome-status: nothing to set — pass a state, or --clear (see --help)");
        return 2;
    }

    let tab_id = match tab_override.or_else(|| std::env::var("_TAB_ID").ok()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            eprintln!("set-rehome-status: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
            return 1;
        }
    };

    let cleared = status.is_none();
    let body = serde_json::json!({ "rehome_status": status }).to_string();
    let url = format!("{api_url}/tabs/by-id/{tab_id}/rehome");
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
                println!("✓ rehome status cleared");
            } else {
                println!("✓ rehome status set");
            }
            0
        }
        // A 400 means an unknown state — surface it so a typo is caught.
        Err(ureq::Error::StatusCode(400)) => {
            eprintln!(
                "set-rehome-status: invalid state (expected one of \
                 handoff-written|successor-ready|ack-sent|safe-to-close)"
            );
            1
        }
        Err(e) => {
            eprintln!("set-rehome-status: {e}");
            1
        }
    }
}
