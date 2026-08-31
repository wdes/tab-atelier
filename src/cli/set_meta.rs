// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-meta <key> <value> [--tab <id>]` / `set-meta <key> --clear`
//!
//! Free-form durable labels on a tab. Unlike `set-context` (one free-text
//! line, "what I'm working on") this is a small key/value map, and unlike
//! `env set --tab` it never reaches the PTY and is never masked — it's
//! labelling, not configuration.
//!
//! We assign no meaning to any key. An orchestration layer on top (roles,
//! project phases, its own bookkeeping) carries its vocabulary here instead of
//! growing a field in the tab model, and reads it back from `tabs --json`
//! after a compaction or a restart.
//!
//! Defaults to the caller's own tab (`_TAB_ID`); `--tab <id>` targets another.
//! Same env contract as `set-status` / `set-context`.

use std::time::Duration;

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
                    eprintln!("set-meta: --tab expects a tab id");
                    return 2;
                };
                tab_override = Some(t.clone());
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-meta [--tab <id>] <key> <value>  |  <key> --clear\n\
                     Attach a free-form durable label to a tab, surfaced in `tabs --json`\n\
                     and on GET /tabs. Keys are yours to choose ([a-z0-9_-], max {k} chars);\n\
                     up to {n} per tab, values up to {v} chars.\n\
                     Examples:\n  \
                       tab-atelier set-meta role reviewer\n  \
                       tab-atelier set-meta project kalpin-back\n  \
                       tab-atelier set-meta role --clear",
                    k = crate::META_KEY_MAX,
                    n = crate::META_MAX_KEYS,
                    v = crate::META_VALUE_MAX,
                );
                return 0;
            }
            other if !other.starts_with("--") => parts.push(other.to_string()),
            other => {
                eprintln!("set-meta: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    let Some((key, rest)) = parts.split_first() else {
        eprintln!("set-meta: expected <key> (see --help)");
        return 2;
    };
    let value = rest.join(" ");
    if value.is_empty() && !clear {
        eprintln!("set-meta: nothing to set — pass a value, or --clear to remove the key");
        return 2;
    }
    // Validate locally so a typo is a clear message instead of a 400 body.
    let key = match crate::sanitize_meta(key, if clear { "x" } else { &value }) {
        Ok((k, _)) => k,
        Err(e) => {
            eprintln!("set-meta: {e}");
            return 2;
        }
    };

    // Outside a tab-atelier tab the API env isn't exported — silent no-op,
    // exactly like `set-status`, so a hook wired to this never blocks.
    let (Ok(api_url), Ok(api_token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) else {
        return 0;
    };
    let tab_id = match tab_override.or_else(|| std::env::var("_TAB_ID").ok()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            eprintln!("set-meta: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
            return 1;
        }
    };

    let body = serde_json::json!({
        "key": key,
        "value": if clear { serde_json::Value::Null } else { serde_json::Value::String(value) },
    })
    .to_string();
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent();
    match agent
        .post(format!("{api_url}/tabs/by-id/{tab_id}/meta"))
        .header("Authorization", &format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .send(&body)
    {
        Ok(_) => {
            if clear {
                println!("✓ meta {key} cleared");
            } else {
                println!("✓ meta {key} set");
            }
            0
        }
        Err(e) => {
            eprintln!("set-meta: {e}");
            1
        }
    }
}
