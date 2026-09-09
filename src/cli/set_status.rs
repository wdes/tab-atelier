// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier set-status <state> [--label …] [--session …] [--kind …] [--plan]`
//!
//! Tiny CLI for tools (catbus-agent, shell hooks, …) running inside a
//! tab-atelier tab to publish a per-tab agent state. Reads `_TAB_ID` from
//! env and finds the API through the shared discovery in
//! [`crate::cli::client`] — env vars first, then the daemon's token file,
//! which is how it reaches an instance running as a system service.
//! Silently no-ops (exit 0) outside a tab so a shell rc file calling it
//! doesn't spam errors.

#[must_use]
pub fn run(args: &[String]) -> i32 {
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

    let mut state: Option<String> = None;
    let mut label: Option<String> = None;
    let mut session: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut plan: Option<bool> = None;
    let mut daemon = false;
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
            // A session-less daemon (`brain`-shaped: a `tab-atelier <verb>` run
            // as its own tab) — restore relaunches the verb instead of dropping
            // to a shell. Announced by the daemon itself at startup.
            "--daemon" => daemon = true,
            "--plan" => plan = Some(true),
            "--no-plan" => plan = Some(false),
            other if state.is_none() && !other.starts_with("--") => {
                state = Some(other.to_string());
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-status <state> [--label L] [--session ID] [--kind K] \
                     [--daemon] [--plan|--no-plan]\n\
                     \n\
                     Publish this tab's agent state, shown as the tab's LED.\n\
                     \n\
                     --label L     free-text label beside the state\n\
                     --session ID  the agent session this belongs to\n\
                     --kind K      which agent (claude, catbus, ...)\n\
                     --daemon      a session-less daemon tab: restore relaunches the verb\n\
                     --plan        the agent is in plan mode (--no-plan clears it)"
                );
                return 0;
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
            "usage: tab-atelier set-status <idle|thinking|waiting|error> [--label …] [--session UUID] [--kind catbus|claude|<daemon>] [--daemon] [--plan|--no-plan]"
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
