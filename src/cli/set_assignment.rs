// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier set-assignment "<phase/role>" [--tab <id>] [--clear]`
//!
//! Declare what a tab is *for*, as opposed to `set-context`'s note about what it is doing right
//! now. The phase groups the tab into a band on the dashboard, and the role is what
//! `dispatch --to <role>` matches — so this is the field a fleet's structure is built out of.
//!
//! Defaults to the caller's own tab (`_TAB_ID`, injected into every PTY); `--tab <id>` targets
//! another tab, which is how an orchestrator files the workers it spawned.

/// `tab-atelier set-assignment [--tab <id>] "<phase[/role]>"` — or `--clear`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier set-assignment",
    about = "Declare what this tab is for (phase/role) — the field the dashboard groups by",
    long_about = "Declare what this tab is for, as `[override:]phase[/role]`. The phase is the \
                  dashboard band; the role is what `dispatch --to <role>` matches. Defaults to \
                  the current tab.",
    after_help = "Examples:\n  \
                  tab-atelier set-assignment build/api\n  \
                  tab-atelier set-assignment review/pr\n  \
                  tab-atelier set-assignment hotfix:build/api --tab <uuid>\n  \
                  tab-atelier set-assignment --clear"
)]
struct Cli {
    /// The assignment: `[override:]phase[/role]`.
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
    /// Which tab; defaults to the caller's own (`_TAB_ID`).
    #[arg(long)]
    tab: Option<String>,
    /// Drop the tab's assignment instead of setting one.
    #[arg(long)]
    clear: bool,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier set-assignment", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Cli {
        text: parts,
        tab: tab_override,
        clear,
    } = cli;

    // Outside a tab-atelier tab the API env isn't exported. A silent no-op (exit 0), exactly like
    // `set-status` and `set-context`, so a hook wired to this can never block a prompt or spam when
    // `claude` runs outside a tab.
    let Ok(ep) = super::client::discover_endpoint() else {
        return 0;
    };

    let assignment: Option<String> = if clear {
        // `--clear` alongside text used to clear and silently discard the argument, which reads as
        // "it set what I typed" when the opposite happened. Refusing is the only honest answer:
        // the caller meant one of the two and only they can say which.
        if !parts.is_empty() {
            eprintln!(
                "set-assignment: --clear discards {}, but an assignment was also given — pass one or the other",
                parts.join(" ")
            );
            return 2;
        }
        None
    } else {
        // Control characters removed, not trimmed: the phase becomes a band name that reaches the
        // dashboard, a log line and possibly a terminal, and an ESC sequence in it would be
        // rendered by whatever displays it. A tab or newline would also break the one-line
        // contract the whole listing depends on.
        let joined = parts.join(" ");
        let s: String = joined.chars().filter(|c| !c.is_control()).collect();
        let s = s.trim().to_owned();
        (!s.is_empty()).then_some(s)
    };
    if assignment.is_none() && !clear {
        eprintln!("set-assignment: nothing to set — pass <phase[/role]>, or --clear (see --help)");
        return 2;
    }

    let tab_id = match tab_override.or_else(|| std::env::var("_TAB_ID").ok()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            eprintln!("set-assignment: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
            return 1;
        }
    };
    // The id lands in a URL *path*, so a `/`, `?`, `#` or `..` in it addresses a different route
    // rather than failing. `_TAB_ID` is ours, but `--tab` is whatever the caller typed, and an id
    // is a bare uuid or index — so anything outside that alphabet is refused instead of escaped:
    // a tab id that needs percent-encoding is a mistake, not a name.
    if !tab_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        eprintln!("set-assignment: --tab must be a tab uuid or index (got {tab_id:?})");
        return 1;
    }

    // Echo what the server parsed, not what was typed: the daemon is what decides whether "build/"
    // means a phase with nobody in it, and a CLI that claimed otherwise would be guessing.
    let body = serde_json::json!({ "assignment": assignment }).to_string();
    match super::client::api_post_json_to(&ep, &format!("/tabs/by-id/{tab_id}/assignment"), body) {
        Ok(parsed) => {
            let shown = parsed
                .get("assignment")
                .and_then(|a| {
                    let phase = a.get("phase")?.as_str()?;
                    if phase.is_empty() {
                        return None;
                    }
                    let role = a.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    let over = a.get("over").and_then(|o| o.as_str());
                    let mut out = String::new();
                    if let Some(o) = over {
                        out.push_str(o);
                        out.push(':');
                    }
                    out.push_str(phase);
                    if !role.is_empty() {
                        out.push('/');
                        out.push_str(role);
                    }
                    Some(out)
                })
                .filter(|s| !s.is_empty());
            match shown {
                Some(s) => println!("✓ assignment set: {s}"),
                None => println!("✓ assignment cleared"),
            }
            0
        }
        Err(e) => {
            eprintln!("set-assignment: {e}");
            1
        }
    }
}
