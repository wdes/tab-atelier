// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The catbus-agent bridge: session metadata, prompt forwarding over the
//! agent's UNIX socket, and the parsed conversation.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn session<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    // Lightweight metadata endpoint — "does this tab have a
    // detectable agent session (Claude Code TUI or
    // catbus-agent), and if so, which file is the transcript
    // living in?". 404 when no candidate process is found
    // under the tab's shell. Accepts both `/tabs/<idx>/catbus`
    // and `/tabs/by-id/<uuid>/catbus` — the UUID is the stable
    // handle (index drifts as tabs open/close), so API clients
    // can address a catbus session by its tab UUID directly.
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

pub(super) fn message<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Forward a user prompt to the tab's catbus-agent over
    // its UNIX socket. Sync — we block here until the agent
    // produces a `done` frame or errors out. The mobile
    // client picks up the appended assistant turn via the
    // existing GET messages endpoint on its next poll.
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
    // Body is `{"text":"…"}` — JSON keeps the door open for
    // future fields (plan-mode toggle, model override, …).
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

pub(super) fn messages<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, query_since: Option<usize>) {
    // Parsed conversation. Skips meta entries (permission
    // mode, file snapshots). Returns the full message list;
    // the mobile remote diffs on its end. `?since=N` lets a
    // client skip the first N messages once incremental
    // updates land.
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
    // parse_messages_since walks the full file and only keeps
    // entries from index `since` onward, so the absolute total
    // is `since + tail.len()`. Same value the client used to see
    // from `all.len()`, without the all-into-memory hop.
    let total = since.saturating_add(tail.len());
    let body = serde_json::to_string(&serde_json::json!({
        "session_id": session.session_id,
        "total": total,
        "messages": tail,
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}
