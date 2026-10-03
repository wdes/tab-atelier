// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier set-status <state> [--label …] [--session …] [--kind …] [--plan]`
//!
//! Tiny CLI for tools (catbus-agent, shell hooks, …) running inside a
//! tab-atelier tab to publish a per-tab agent state. Reads `_TAB_ID` from
//! env and finds the API through the shared discovery in
//! [`crate::cli::client`] — env vars first, then the daemon's token file,
//! which is how it reaches an instance running as a system service.
//! Silently no-ops (exit 0) outside a tab so a shell rc file calling it
//! doesn't spam errors.

/// `tab-atelier set-status <state> [--label …] [--session …] …`
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier set-status",
    about = "Publish this tab's agent state, shown as the tab's LED"
)]
struct Cli {
    /// `idle`, `thinking`, `waiting`, `error`, …
    state: String,
    /// Free-text label beside the state.
    #[arg(long)]
    label: Option<String>,
    /// The agent session this belongs to.
    #[arg(long)]
    session: Option<String>,
    /// Which agent (`claude`, `catbus`, …).
    #[arg(long)]
    kind: Option<String>,
    /// A session-less daemon tab (`brain`-shaped: a `tab-atelier <verb>` run
    /// as its own tab) — restore relaunches the verb instead of dropping to a
    /// shell. Announced by the daemon itself at startup.
    #[arg(long)]
    daemon: bool,
    /// The agent is in plan mode.
    #[arg(long)]
    plan: bool,
    /// The agent is no longer in plan mode.
    #[arg(long, conflicts_with = "plan")]
    no_plan: bool,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    // Arguments first, endpoint second. The other order meant `set-status
    // --help` outside a tab exited 0 having printed nothing, because the
    // silent no-op fired before anything looked at the arguments.
    let cli = match super::parse::<Cli>("tab-atelier set-status", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Ok(tab_id) = std::env::var("_TAB_ID") else {
        // Outside a tab-atelier tab — silent no-op.
        return 0;
    };
    // No endpoint at all — silent no-op, as above. Discovery covers the
    // token file too, so this no longer misses a daemon that runs as a
    // service and never exported the env vars.
    let Ok(ep) = super::client::discover_endpoint() else {
        return 0;
    };
    let Cli {
        state,
        label,
        session,
        kind,
        daemon,
        plan,
        no_plan,
    } = cli;
    let plan = if plan {
        Some(true)
    } else if no_plan {
        Some(false)
    } else {
        None
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
    if daemon {
        body.insert("daemon".into(), serde_json::Value::Bool(true));
    }
    let body = serde_json::Value::Object(body).to_string();

    match super::client::api_post_to(&ep, &format!("/tabs/by-id/{tab_id}/status"), body) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tab-atelier set-status: {e}");
            1
        }
    }
}
