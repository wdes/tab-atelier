// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-tab agent-state hook (`set-status`): thinking/waiting/error/idle,
//! plus session/kind/plan/daemon metadata, queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::{PendingStatusUpdate, TabSnapshot, error_json, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Per-tab agent state hook. Looked up by stable UUID
    // (`_TAB_ID` env var) rather than position, so a rename
    // doesn't break the mapping.
    let tab_id = &p["/tabs/by-id/".len()..p.len() - "/status".len()];
    if tab_id.is_empty() {
        error_json(stream, 404, "missing tab id");
        return;
    }
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(state_str) = parsed.get("state").and_then(|v| v.as_str()) else {
        error_json(stream, 400, "missing `state` field");
        return;
    };
    let agent_state = match state_str {
        "thinking" => crate::AgentState::Thinking,
        "waiting" => crate::AgentState::Waiting,
        "error" => crate::AgentState::Error,
        "idle" => {
            // "idle" = clear the indicator. Queue an Error-shaped
            // marker the loop interprets as "wipe"; simpler than
            // adding a fourth enum variant just for the wire.
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(t) = snap.tabs.iter().find(|t| &*t.id == tab_id) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let id = t.id.clone();
            snap.pending_status_updates.push(PendingStatusUpdate {
                tab_id: id.to_string(),
                state: crate::AgentState::Thinking, // ignored — clear flag below
                label: Some("__clear__".into()),
                session_id: None,
                agent_kind: None,
                plan_mode: None,
                daemon: None,
            });
            drop(snap);
            respond_json(stream, 200, r#"{"cleared":true}"#);
            return;
        }
        _ => {
            error_json(stream, 400, "invalid state (idle/thinking/waiting/error)");
            return;
        }
    };
    let label = parsed
        .get("label")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);
    let session_id = parsed
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);
    let agent_kind = parsed
        .get("agentKind")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);
    let plan_mode = parsed.get("planMode").and_then(serde_json::Value::as_bool);
    let daemon = parsed.get("daemon").and_then(serde_json::Value::as_bool);
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(t) = snap.tabs.iter().find(|t| &*t.id == tab_id) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = t.id.clone();
    info!(
        "API: set-status tab={id} state={state_str} session={} kind={}",
        session_id.as_deref().unwrap_or("-"),
        agent_kind.as_deref().unwrap_or("-")
    );
    snap.pending_status_updates.push(PendingStatusUpdate {
        tab_id: id.to_string(),
        state: agent_state,
        label,
        session_id,
        agent_kind,
        plan_mode,
        daemon,
    });
    drop(snap);
    respond_json(stream, 200, r#"{"ok":true}"#);
}
