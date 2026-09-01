// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab agent-card route handlers: status hook, context/assignment/parent/
//! rehome fields, the generic set-* card verbs, evaluation append, usage bump.
//! Bodies moved verbatim from `handle_connection`'s match arms
//! (behavior-preserving). Looked up by stable UUID, RW/master token upstream.

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::super::{
    CardChange, PendingStatusUpdate, TabSnapshot, card_route_verb, error_json, is_rehome_state, respond_json,
};

/// `POST /tabs/by-id/{uuid}/status` — per-tab agent state hook.
pub(in crate::api) fn status<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
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
            // "idle" = clear the indicator. Queue an Error-shaped marker the loop
            // interprets as "wipe"; simpler than a fourth enum variant.
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
    });
    drop(snap);
    respond_json(stream, 200, r#"{"ok":true}"#);
}

/// `POST /tabs/by-id/{uuid}/context` — set/clear the tab's free-text context.
pub(in crate::api) fn context<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/context".len()];
    let context_opt: Option<String> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("context").cloned())
            .and_then(|c| {
                if c.is_null() {
                    None
                } else {
                    c.as_str().map(str::to_owned)
                }
            })
    };
    // Cap length so a runaway agent can't bloat the snapshot; trim-only → clear.
    let context_opt = context_opt
        .map(|s| s.chars().take(2000).collect::<String>())
        .filter(|s| !s.trim().is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].context = context_opt.as_deref().map(std::sync::Arc::from);
    state.pending_context_changes.push((tab_id, context_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "context": context_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/assignment` — set/clear the stable workflow assignment.
pub(in crate::api) fn assignment<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    body_bytes: &[u8],
) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/assignment".len()];
    let assignment_opt: Option<String> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("assignment").cloned())
            .and_then(|c| {
                if c.is_null() {
                    None
                } else {
                    c.as_str().map(str::to_owned)
                }
            })
    };
    let assignment_opt = assignment_opt
        .map(|s| s.chars().take(2000).collect::<String>())
        .filter(|s| !s.trim().is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].assignment = assignment_opt.as_deref().map(std::sync::Arc::from);
    state.pending_assignment_changes.push((tab_id, assignment_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "assignment": assignment_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/parent` — stamp a spawned tab's lineage.
pub(in crate::api) fn parent<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/parent".len()];
    let parent_opt: Option<String> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("parent_tab_id").cloned())
            .and_then(|c| {
                if c.is_null() {
                    None
                } else {
                    c.as_str().map(str::to_owned)
                }
            })
    };
    let parent_opt = parent_opt
        .map(|s| s.chars().take(128).collect::<String>())
        .filter(|s| !s.trim().is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].parent_tab_id = parent_opt.as_deref().map(std::sync::Arc::from);
    state.pending_parent_changes.push((tab_id, parent_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "parent_tab_id": parent_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/rehome` — set/clear a predecessor's re-home progress.
pub(in crate::api) fn rehome<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/rehome".len()];
    let rehome_opt: Option<String> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("rehome_status").cloned())
            .and_then(|c| {
                if c.is_null() {
                    None
                } else {
                    c.as_str().map(str::to_owned)
                }
            })
    };
    let rehome_opt = rehome_opt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(s) = &rehome_opt
        && !is_rehome_state(s)
    {
        error_json(stream, 400, "invalid rehome_status");
        return;
    }
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].rehome_status = rehome_opt.as_deref().map(std::sync::Arc::from);
    state.pending_rehome_changes.push((tab_id, rehome_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "rehome_status": rehome_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/{verb}` — the generic set-* card verbs (specialty/
/// orchestrator/objective OVERWRITE, current-task APPENDS, conventions OVERWRITE,
/// rounds-active toggles). Verb resolved by `card_route_verb`.
pub(in crate::api) fn card_verb<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let (verb, json_key) = card_route_verb(p).unwrap_or(("", ""));
    let inner = &p["/tabs/by-id/".len()..p.len() - verb.len() - 1];
    let raw: Option<String> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get(json_key).cloned())
            .and_then(|c| {
                if c.is_null() {
                    None
                } else {
                    c.as_str().map(str::to_owned)
                }
            })
    };
    // Same length cap as /assignment / /context.
    let raw = raw.map(|s| s.chars().take(2000).collect::<String>());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let change = match verb {
        "specialty" => {
            let v = raw.filter(|s| !s.trim().is_empty());
            state.tabs[idx].specialty = v.as_deref().map(std::sync::Arc::from);
            CardChange::Specialty(v)
        }
        "orchestrator" => {
            let v = raw.filter(|s| !s.trim().is_empty());
            state.tabs[idx].orchestrator = v.as_deref().map(std::sync::Arc::from);
            CardChange::Orchestrator(v)
        }
        "objective" => {
            let v = raw.filter(|s| !s.trim().is_empty());
            state.tabs[idx].objective = v.as_deref().map(std::sync::Arc::from);
            CardChange::Objective(v)
        }
        "current-task" => {
            let Some(phrase) = raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
                // Empty phrase → no-op (the permalog stays meaningful).
                drop(state);
                respond_json(stream, 200, r#"{"currentTask":"noop"}"#);
                return;
            };
            crate::append_current_task(&mut state.tabs[idx].current_task, &phrase);
            CardChange::CurrentTaskAppend(phrase)
        }
        "conventions" => {
            // OVERWRITE the declared .md list (empty/None clears it).
            let list = raw.as_deref().map(crate::parse_conventions).unwrap_or_default();
            state.tabs[idx].conventions.clone_from(&list);
            CardChange::Conventions(list)
        }
        "spawn-mode" => {
            // SV4: `fresh` / `resume` (anything else clears it). Stamped by
            // `spawn --from-skill` so the retire record carries the A/B partition key.
            let mode = match raw.as_deref().map(str::trim) {
                Some("fresh") => Some(crate::cli::catalog::SpawnMode::Fresh),
                Some("resume") => Some(crate::cli::catalog::SpawnMode::Resume),
                Some("origin") => Some(crate::cli::catalog::SpawnMode::Origin),
                _ => None,
            };
            state.tabs[idx].spawn_mode = mode;
            CardChange::SpawnMode(mode)
        }
        // rounds-active
        _ => {
            let active = raw.as_deref().is_some_and(|s| matches!(s.trim(), "true" | "1" | "on"));
            let ra = crate::RoundsActive {
                active,
                last_round_at: active.then(crate::unix_millis),
            };
            state.tabs[idx].rounds_active = Some(ra.clone());
            CardChange::RoundsActive(ra)
        }
    };
    state.pending_card_changes.push((tab_id, change));
    drop(state);
    respond_json(stream, 200, r#"{"ok":true}"#);
}

/// `POST /tabs/by-id/{uuid}/evaluation` — append one evaluation record.
pub(in crate::api) fn evaluation<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    body_bytes: &[u8],
) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/evaluation".len()];
    let Ok(ev) = serde_json::from_slice::<crate::Evaluation>(body_bytes) else {
        error_json(stream, 400, "invalid evaluation record");
        return;
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    crate::append_evaluation(&mut state.tabs[idx].evaluations, ev.clone());
    state
        .pending_card_changes
        .push((tab_id, CardChange::EvaluationAppend(ev)));
    drop(state);
    respond_json(stream, 200, r#"{"ok":true}"#);
}

/// `POST /tabs/by-id/{uuid}/bump-usage` — bump the usage counter + stamp last-used.
pub(in crate::api) fn bump_usage<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/bump-usage".len()];
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let (count, stamp) = crate::bump_usage(state.tabs[idx].usage_count, crate::unix_millis());
    state.tabs[idx].usage_count = Some(count);
    state.tabs[idx].last_used_at = Some(stamp);
    state
        .pending_card_changes
        .push((tab_id, CardChange::Usage(count, stamp)));
    drop(state);
    respond_json(stream, 200, &format!(r#"{{"usageCount":{count}}}"#));
}
