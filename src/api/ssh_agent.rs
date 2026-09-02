// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab ssh-agent enable/disable. Headless-only: the GUI edition returns
//! 501 (the daemon owns the agent lifecycle).

use std::io::Write;
use std::sync::{Arc, Mutex};

#[cfg(not(feature = "gui"))]
use super::respond_json;
use super::{TabSnapshot, error_json};

// The params are unused in the GUI edition, which only 501s.
#[cfg_attr(feature = "gui", allow(unused_variables))]
pub(super) fn set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Enable/disable a per-tab ssh-agent. Master token only (same
    // gate as /net). Body: `{"enabled": true, "key": "/path/to/key"}`
    // to enable (key optional, must be passphrase-less to auto-load);
    // `{"enabled": false}` to disable and reap the agent. The shell
    // respawns to apply, so it's not instantaneous.
    //
    // The agent lifecycle is owned by the headless daemon; the GUI
    // spawn path isn't wired for it, so the GUI returns 501 and never
    // drains `pending_ssh_agent_changes`.
    #[cfg(feature = "gui")]
    error_json(
        stream,
        501,
        "per-tab ssh-agent requires the headless daemon; the desktop GUI does not manage per-tab agents",
    );
    #[cfg(not(feature = "gui"))]
    {
        let inner = &p["/tabs/by-id/".len()..p.len() - "/ssh-agent".len()];
        let val: serde_json::Value = if body_bytes.is_empty() {
            serde_json::json!({})
        } else {
            let Ok(v) = serde_json::from_slice(body_bytes) else {
                error_json(stream, 400, "invalid JSON body");
                return;
            };
            v
        };
        // Default enabled=true when the body omits it, so a bare
        // `ssh-agent <tab>` enables; explicit `false` disables.
        let enabled = val.get("enabled").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let key = val.get("key").and_then(serde_json::Value::as_str).map(str::to_string);
        let config = enabled.then_some(crate::SshAgentConfig { key });
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
            drop(state);
            error_json(stream, 404, "tab not found");
            return;
        };
        let tab_id = state.tabs[idx].id.to_string();
        state.pending_ssh_agent_changes.push((tab_id, config));
        drop(state);
        let body = serde_json::to_string(&serde_json::json!({"ssh_agent": enabled})).unwrap_or_default();
        respond_json(stream, 200, &body);
    }
}
