// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/tabs/…/catbus[/message|/messages]` route handlers. Bodies moved verbatim
//! from `handle_connection`'s match arms (behavior-preserving). Whole module is
//! gated on the `catbus` feature, like the arms it came from.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

/// `GET /tabs/…/catbus` — lightweight session metadata (does this tab have a
/// detectable agent session, and which transcript file). Accepts index or UUID.
pub(in crate::api) fn session_meta<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let pid = t.shell_pid;
    drop(snap);
    match crate::catbus_agent::find_session(pid) {
        Some(session) => {
            let body = serde_json::to_string(&serde_json::json!({
                "session_id": session.session_id,
                "agent_pid": session.agent_pid,
                "cwd": session.cwd.to_string_lossy(),
                "file": session.file_path.to_string_lossy(),
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        None => error_json(stream, 404, "no agent session under this tab"),
    }
}

/// `POST /tabs/…/catbus/message` — forward a user prompt to the tab's catbus-
/// agent over its UNIX socket, blocking until a `done`/error frame.
pub(in crate::api) fn send_message<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    body_bytes: &[u8],
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus/message") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let pid = t.shell_pid;
    drop(snap);
    let Some(session) = crate::catbus_agent::find_session(pid) else {
        error_json(stream, 404, "no agent session under this tab");
        return;
    };
    let socket_path = session.file_path.with_extension("sock");
    // Body is `{"text":"…"}` — JSON keeps the door open for future fields.
    let req: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
        error_json(stream, 400, "missing `text` field");
        return;
    };
    match crate::catbus_agent::send_prompt_to_socket(&socket_path, text) {
        Ok(reply) => {
            let body = serde_json::to_string(&serde_json::json!({
                "session_id": session.session_id,
                "reply": reply,
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        Err(e) => error_json(stream, 502, &format!("agent socket: {e}")),
    }
}

/// `GET /tabs/…/catbus/messages[?since=N]` — the parsed conversation tail.
pub(in crate::api) fn messages<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    query_since: Option<usize>,
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus/messages") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let pid = t.shell_pid;
    drop(snap);
    let Some(session) = crate::catbus_agent::find_session(pid) else {
        error_json(stream, 404, "no agent session under this tab");
        return;
    };
    let since = query_since.unwrap_or(0);
    let tail = crate::catbus_agent::parse_messages_since(&session.file_path, since);
    // Absolute total = since + tail.len() (parse_messages_since keeps entries
    // from index `since` onward), without the all-into-memory hop.
    let total = since.saturating_add(tail.len());
    let body = serde_json::to_string(&serde_json::json!({
        "session_id": session.session_id,
        "total": total,
        "messages": tail,
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}
