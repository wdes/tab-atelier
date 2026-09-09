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

/// `tab-atelier set-meta [--tab <id>] <key> <value>` — or `<key> --clear`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier set-meta",
    about = "Attach a free-form durable label to a tab",
    long_about = "Attach a free-form durable label to a tab, surfaced in `tabs --json` and on \
                  GET /tabs. Keys are yours to choose ([a-z0-9_-]); several words of value are \
                  joined with spaces.",
    after_help = "Examples:\n  \
                  tab-atelier set-meta role reviewer\n  \
                  tab-atelier set-meta project kalpin-back\n  \
                  tab-atelier set-meta role --clear"
)]
struct Cli {
    /// The key, then its value. `<key>` alone is only valid with `--clear`.
    #[arg(trailing_var_arg = true)]
    parts: Vec<String>,
    /// Which tab; defaults to the caller's own (`_TAB_ID`).
    #[arg(long)]
    tab: Option<String>,
    /// Remove the key instead of setting it.
    #[arg(long)]
    clear: bool,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier set-meta", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Cli {
        parts,
        tab: tab_override,
        clear,
    } = cli;

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
    // Discovery covers the daemon's token file as well as the env vars, so
    // this now works against an instance running as a system service — which
    // it did not when it read the environment alone.
    let Ok(ep) = super::client::discover_endpoint() else {
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
    match super::client::api_post_to(&ep, &format!("/tabs/by-id/{tab_id}/meta"), body) {
        Ok(()) => {
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
    fn set_meta_reaches_a_daemon_the_environment_never_mentioned() {
        // This verb used to read TAB_ATELIER_API_URL/TOKEN straight from the
        // environment, so on a machine where the daemon runs as a service —
        // env vars unexported, token in a file — it returned 0 having done
        // nothing at all. Silent success is the worst possible answer there.
        //
        // Going through `discover_endpoint()` fixes that, and this test is
        // what proves it: the harness injects an endpoint through discovery
        // and NEVER sets the env vars, so it can only pass if the verb asks
        // discovery rather than the environment.
        crate::cli::share_link::with_test_server(|state| {
            let uuid = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tabs
                .first()
                .map(|t| t.id.to_string())
                .expect("harness tab");
            assert_eq!(
                super::run(&margs(&["--tab", &uuid, "role", "reviewer"])),
                0,
                "set-meta must reach the discovered daemon"
            );
            // Asserting the EFFECT, not the exit code: the old behaviour also
            // returned 0 here, because "no API env" was a silent no-op. Only
            // a queued change proves the request actually arrived.
            let queued = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_meta_changes
                .clone();
            let mine = queued
                .iter()
                .find(|m| m.tab_id == uuid && m.key == "role")
                .expect("set-meta reached the daemon but queued nothing");
            assert_eq!(mine.value.as_deref(), Some("reviewer"));
        });
    }

    #[test]
    fn set_meta_checks_its_arguments_before_looking_for_a_daemon() {
        // Runs before any endpoint work, which is what makes it safe to
        // assert on without a server.
        assert_eq!(super::run(&margs(&["--tab", "tab-a"])), 2, "a tab with no key/value");
    }
}
