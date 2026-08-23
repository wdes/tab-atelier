// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-status <state> [--label …] [--session …] [--kind …] [--plan]`
//!
//! Tiny CLI for tools (catbus-agent, shell hooks, …) running inside a
//! tab-atelier tab to publish a per-tab agent state. Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env. Silently
//! no-ops (exit 0) when those aren't set so a shell rc file calling
//! it outside a tab doesn't spam errors.

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let Ok(tab_id) = std::env::var("_TAB_ID") else {
        // Outside a tab-atelier tab — silent no-op.
        return 0;
    };
    let Ok(api_url) = std::env::var("TAB_ATELIER_API_URL") else {
        return 0;
    };
    let Ok(api_token) = std::env::var("TAB_ATELIER_API_TOKEN") else {
        return 0;
    };

    let mut state: Option<String> = None;
    let mut label: Option<String> = None;
    let mut session: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut plan: Option<bool> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--label" => {
                i += 1;
                label = args.get(i).cloned();
            }
            "--session" => {
                i += 1;
                session = args.get(i).cloned();
            }
            "--kind" => {
                i += 1;
                kind = args.get(i).cloned();
            }
            "--plan" => plan = Some(true),
            "--no-plan" => plan = Some(false),
            other if state.is_none() && !other.starts_with("--") => {
                state = Some(other.to_string());
            }
            other => {
                eprintln!("tab-atelier set-status: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    let Some(state) = state else {
        eprintln!(
            "usage: tab-atelier set-status <idle|thinking|waiting|error> [--label …] [--session UUID] [--kind catbus|claude] [--plan|--no-plan]"
        );
        return 2;
    };

    let mut body = serde_json::Map::new();
    body.insert("state".into(), serde_json::Value::String(state));
    if let Some(v) = label {
        body.insert("label".into(), serde_json::Value::String(v));
    }
    if let Some(v) = session {
        body.insert("sessionId".into(), serde_json::Value::String(v));
    }
    if let Some(v) = kind {
        body.insert("agentKind".into(), serde_json::Value::String(v));
    }
    if let Some(v) = plan {
        body.insert("planMode".into(), serde_json::Value::Bool(v));
    }
    let body = serde_json::Value::Object(body).to_string();

    // Same HTTP tail as the single-field CLIs (shared to kill the dup).
    match super::tab_field::send(&api_url, &api_token, &tab_id, "status", &body) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("tab-atelier set-status: {e}");
            1
        }
    }
}
