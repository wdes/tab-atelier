// SPDX-License-Identifier: MPL-2.0

//! Per-tab ssh-agent enable/disable. Headless-only: the GUI edition returns
//! 501 (the daemon owns the agent lifecycle).

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::TabSnapshot;

/// Whether the agent is now on, echoing what was asked for.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AgentState {
    ssh_agent: bool,
}

pub(super) fn set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<AgentState>, Failure> {
    // Enable/disable a per-tab ssh-agent. Master token only (same
    // gate as /net). Body: `{"enabled": true, "key": "/path/to/key"}`
    // to enable (key optional, must be passphrase-less to auto-load);
    // `{"enabled": false}` to disable and reap the agent. The shell
    // respawns to apply, so it's not instantaneous.
    //
    // The agent lifecycle is owned by the headless daemon; the GUI
    // spawn path isn't wired for it, so the GUI refuses and never
    // drains `pending_ssh_agent_changes`.
    #[cfg(feature = "gui")]
    {
        let _ = (request, state);
        Err(Failure::problem(
            501,
            "per-tab ssh-agent requires the headless daemon; the desktop GUI does not manage per-tab agents",
        ))
    }
    #[cfg(not(feature = "gui"))]
    {
        // `strip_*` rather than byte offsets: the dispatcher's `starts_with` +
        // `ends_with` guard also admits "/tabs/by-id/ssh-agent", whose tail is
        // inside the prefix, so an offset slice would pan out of bounds.
        let Some(inner) = request
            .path
            .strip_prefix("/tabs/by-id/")
            .and_then(|rest| rest.strip_suffix("/ssh-agent"))
        else {
            return Err(Failure::problem(404, "missing tab id"));
        };
        let val: serde_json::Value = if request.body.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(request.body).map_err(|_| Failure::problem(400, "invalid JSON body"))?
        };
        // Default enabled=true when the body omits it, so a bare
        // `ssh-agent <tab>` enables; explicit `false` disables.
        let enabled = val.get("enabled").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let key = val.get("key").and_then(serde_json::Value::as_str).map(str::to_string);
        let config = enabled.then_some(crate::SshAgentConfig { key });
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
            return Err(Failure::problem(404, "tab not found"));
        };
        let tab_id = state.tabs[idx].id.to_string();
        state.pending_ssh_agent_changes.push((tab_id, config));
        drop(state);
        Ok(Doc::ok(AgentState { ssh_agent: enabled }))
    }
}
