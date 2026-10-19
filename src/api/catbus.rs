// SPDX-License-Identifier: MPL-2.0

//! The catbus-agent bridge: session metadata, prompt forwarding over the
//! agent's UNIX socket, and the parsed conversation.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::{TabSnapshot, parse_tab_key, resolve_tab_idx};

/// The agent session under a tab, and where its transcript lives.
///
/// The Rust field is `id` and the wire field is `session_id`: clippy's
/// `struct_field_names` is right that a field repeating its own struct's name is
/// noise to read, but the JSON name is a contract with the mobile client and the
/// viewer, so it is pinned here rather than changed.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Session {
    #[serde(rename = "session_id")]
    id: String,
    agent_pid: u32,
    cwd: String,
    file: String,
}

/// The agent's answer to a forwarded prompt.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Reply {
    #[serde(rename = "session_id")]
    id: String,
    reply: String,
}

/// The parsed conversation, and how much of it there was.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Messages {
    #[serde(rename = "session_id")]
    id: String,
    /// Absolute count, so a client resuming with `?since=N` can tell how far
    /// behind it is without walking the file itself.
    total: usize,
    /// `messages` on the wire — see [`Session::id`] for why the Rust name is
    /// not the JSON one.
    #[serde(rename = "messages")]
    entries: Vec<serde_json::Value>,
}

/// The tab id and the live agent session under it, or the refusal to give.
///
/// All three routes begin with the same four steps — parse the key, find the
/// tab, read its shell pid, locate the agent — and each used to spell them out,
/// including the same three 404 messages. One helper means a route can no longer
/// disagree with its siblings about what "no agent under this tab" means.
fn session_of(
    request: &Request<'_>,
    state: &Arc<Mutex<TabSnapshot>>,
    suffix: &str,
) -> Result<(String, crate::catbus_agent::AgentSession), Failure> {
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, suffix) else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let pid = shell_pid_of(state, key_raw, is_uuid)?;
    crate::catbus_agent::find_session(pid)
        .map(|session| (key_raw.to_string(), session))
        .ok_or_else(|| Failure::problem(404, "no agent session under this tab"))
}

/// The shell pid under one tab, with the snapshot lock held for the read alone.
///
/// A separate function so the guard drops at its return rather than by
/// convention: what follows in [`session_of`] walks the filesystem for an agent
/// transcript, which can be slow enough that holding the snapshot through it
/// would stall every other request on the box.
fn shell_pid_of(state: &Arc<Mutex<TabSnapshot>>, key_raw: &str, is_uuid: bool) -> Result<u32, Failure> {
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let Some(t) = snap.tabs.get(idx) else {
        return Err(Failure::problem(404, "tab index out of range"));
    };
    let pid = t.shell_pid;
    drop(snap);
    Ok(pid)
}

pub(super) fn session_request(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Session>, Failure> {
    // Lightweight metadata endpoint — "does this tab have a
    // detectable agent session (Claude Code TUI or
    // catbus-agent), and if so, which file is the transcript
    // living in?". 404 when no candidate process is found
    // under the tab's shell. Accepts both `/tabs/<idx>/catbus`
    // and `/tabs/by-id/<uuid>/catbus` — the UUID is the stable
    // handle (index drifts as tabs open/close), so API clients
    // can address a catbus session by its tab UUID directly.
    let (_tab, session) = session_of(request, state, "/catbus")?;
    Ok(Doc::ok(Session {
        id: session.session_id,
        agent_pid: session.agent_pid,
        cwd: session.cwd.to_string_lossy().into_owned(),
        file: session.file_path.to_string_lossy().into_owned(),
    }))
}

pub(super) fn message(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Reply>, Failure> {
    // Forward a user prompt to the tab's catbus-agent over
    // its UNIX socket. Sync — we block here until the agent
    // produces a `done` frame or errors out. The mobile
    // client picks up the appended assistant turn via the
    // existing GET messages endpoint on its next poll.
    let (_tab, session) = session_of(request, state, "/catbus/message")?;
    let socket_path = session.file_path.with_extension("sock");
    // Body is `{"text":"…"}` — JSON keeps the door open for
    // future fields (plan-mode toggle, model override, …).
    let req: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
        return Err(Failure::problem(400, "missing `text` field"));
    };
    let reply = crate::catbus_agent::send_prompt_to_socket(&socket_path, text)
        .map_err(|e| Failure::problem(502, format!("agent socket: {e}")))?;
    Ok(Doc::ok(Reply {
        id: session.session_id,
        reply,
    }))
}

pub(super) fn messages_request(
    request: &Request<'_>,
    state: &Arc<Mutex<TabSnapshot>>,
) -> Result<Doc<Messages>, Failure> {
    // Parsed conversation. Skips meta entries (permission
    // mode, file snapshots). Returns the full message list;
    // the mobile remote diffs on its end. `?since=N` lets a
    // client skip the first N messages once incremental
    // updates land.
    let (_tab, session) = session_of(request, state, "/catbus/messages")?;
    let since = request
        .query_value("since")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0);
    let tail = crate::catbus_agent::parse_messages_since(&session.file_path, since);
    // parse_messages_since walks the full file and only keeps
    // entries from index `since` onward, so the absolute total
    // is `since + tail.len()`. Same value the client used to see
    // from `all.len()`, without the all-into-memory hop.
    let total = since.saturating_add(tail.len());
    Ok(Doc::ok(Messages {
        id: session.session_id,
        total,
        entries: tail.into_iter().filter_map(|m| serde_json::to_value(m).ok()).collect(),
    }))
}
