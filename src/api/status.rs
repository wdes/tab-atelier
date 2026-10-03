// SPDX-License-Identifier: MPL-2.0

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
    //
    // `strip_*` and not byte offsets: the dispatcher's `starts_with` +
    // `ends_with` guard also admits "/tabs/by-id/status", whose tail is
    // inside the prefix, so `p[12..p.len() - 7]` would slice backwards
    // and panic.
    let Some(tab_id) = p
        .strip_prefix("/tabs/by-id/")
        .and_then(|rest| rest.strip_suffix("/status"))
        .filter(|id| !id.is_empty())
    else {
        error_json(stream, 404, "missing tab id");
        return;
    };
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
        "thinking" => Some(crate::AgentState::Thinking),
        "waiting" => Some(crate::AgentState::Waiting),
        "error" => Some(crate::AgentState::Error),
        // "idle" takes the indicator down without touching the durable
        // attachment, so a caller can say "not working any more" and "here is
        // the session to resume" at once. A bare idle carries no metadata and
        // therefore leaves the attachment exactly as it was; only the
        // `__clear__` label detaches (see `WIPE_LABEL`).
        "idle" => None,
        _ => {
            error_json(stream, 400, "invalid state (idle/thinking/waiting/error)");
            return;
        }
    };
    let raw_label = parsed.get("label").and_then(|v| v.as_str());
    // `__clear__` is a verb, not text — see `crate::api::WIPE_LABEL`.
    let wipe_attachment = raw_label == Some(super::WIPE_LABEL);
    // A label belongs to a visible indicator, so it is dropped for a
    // parked one rather than stored where nothing renders it.
    let label = if agent_state.is_some() && !wipe_attachment {
        raw_label.map(std::string::ToString::to_string)
    } else {
        None
    };
    let (session_id, agent_kind, plan_mode, daemon) = if wipe_attachment {
        (None, None, None, None)
    } else {
        (
            parsed
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            parsed
                .get("agentKind")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            parsed.get("planMode").and_then(serde_json::Value::as_bool),
            parsed.get("daemon").and_then(serde_json::Value::as_bool),
        )
    };
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
        wipe_attachment,
    });
    drop(snap);
    respond_json(
        stream,
        200,
        if wipe_attachment {
            r#"{"cleared":true}"#
        } else {
            r#"{"ok":true}"#
        },
    );
}
