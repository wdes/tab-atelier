// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

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

/// `tab-atelier set-context [--tab <id>] "<text>"` — or `--clear`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier set-context",
    about = "Declare what this tab is working on (PR/issue/task)",
    long_about = "Declare what this tab is working on (PR/issue/task). Shows as a hover \
                  tooltip on the GUI tab name and on /tabs. Defaults to the current tab.",
    after_help = "Examples:\n  \
                  tab-atelier set-context \"PR #3719: dompdf font reproduction\"\n  \
                  tab-atelier set-context --clear"
)]
struct Cli {
    /// The context text. Several words are joined with spaces.
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
    /// Which tab; defaults to the caller's own (`_TAB_ID`).
    #[arg(long)]
    tab: Option<String>,
    /// Drop the tab's context instead of setting one.
    #[arg(long)]
    clear: bool,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier set-context", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Cli {
        text: parts,
        tab: tab_override,
        clear,
    } = cli;

    // Outside a tab-atelier tab the API env isn't exported. Treat that
    // as a silent no-op (exit 0) — exactly like `set-status` — so a
    // UserPromptSubmit / SessionEnd hook wired to this can never block
    // prompt submission or spam errors when `claude` runs outside any
    // tab. Once the env IS present we surface real failures normally.
    // Discovery covers the daemon's token file as well as the env vars, so
    // this now works against an instance running as a system service — which
    // it did not when it read the environment alone.
    let Ok(ep) = super::client::discover_endpoint() else {
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
    match super::client::api_post_to(&ep, &format!("/tabs/by-id/{tab_id}/context"), body) {
        Ok(()) => {
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
