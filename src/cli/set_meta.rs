// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

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

#[cfg(test)]
mod tests {
    fn margs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_label_needs_both_a_key_and_a_value() {
        // These fail before any request. A `set-meta` that silently did
        // nothing would leave an orchestrator believing a tab was labelled.
        assert_eq!(super::run(&margs(&[])), 2, "no key");
        assert_eq!(super::run(&margs(&["role"])), 2, "key with no value");
        assert_eq!(super::run(&margs(&["--tab"])), 2, "flag with no value");
        assert_eq!(super::run(&margs(&["--nope", "x"])), 2, "unknown flag");
        // A key that cannot be stored must be refused here rather than by the
        // daemon, so the error names the argument the user typed.
        assert_eq!(super::run(&margs(&["bad key!", "value"])), 2);
        assert_eq!(super::run(&margs(&["", "value"])), 2);
    }

    #[test]
    fn set_meta_talks_to_the_environment_not_the_discovered_endpoint() {
        // Deliberately NOT server-backed. Unlike every other verb, this one
        // reads TAB_ATELIER_API_URL/TOKEN straight from the environment
        // instead of `discover_endpoint()`, so the test harness cannot
        // redirect it — a test that "used the fake daemon" would in fact be
        // posting to the developer's real one.
        //
        // Two consequences worth knowing:
        //
        //  * `--tab` must be a UUID. The request goes to
        //    `/tabs/by-id/{tab}/meta` with no name or index resolution, so
        //    `--tab build` 404s where `dispatch --to build` works.
        //  * Outside a tab (no API env at all) it returns 0 without doing
        //    anything, which keeps it harmless in a hook but means success
        //    does not imply a label was stored.
        //
        // Only the argument checks are exercised here; they run before any of
        // that.
        assert_eq!(super::run(&margs(&["--tab", "tab-a"])), 2, "a tab with no key/value");
    }
}
