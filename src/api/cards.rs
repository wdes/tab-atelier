// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab agent-card route setters: the persisted, hook-immune fields —
//! assignment, spawn lineage (`parent`), re-home progress, the generic `set-*`
//! verbs (specialty/orchestrator/objective/current-task/rounds-active/conventions),
//! evaluation append, and the usage bump. Each reflects into the snapshot and
//! queues the runtime+persist sync drained by the owner loop. Master token only.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{CardChange, TabSnapshot, card_route_verb, error_json, is_rehome_state, respond_json};

/// Parse a `{"<key>": "<value>"|null}` body into `Some(value)` / `None` (missing,
/// empty body, or explicit null all read as clear). Trailing whitespace kept for
/// the caller to cap/trim as each field needs.
fn parse_field(body_bytes: &[u8], key: &str) -> Option<String> {
    if body_bytes.is_empty() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(body_bytes)
        .ok()
        .and_then(|v| v.get(key).cloned())
        .and_then(|c| {
            if c.is_null() {
                None
            } else {
                c.as_str().map(str::to_owned)
            }
        })
}

/// `POST /tabs/by-id/{uuid}/assignment` — set/clear the stable workflow assignment.
pub(super) fn assignment<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/assignment".len()];
    let value = parse_field(body_bytes, "assignment")
        .map(|s| s.chars().take(2000).collect::<String>())
        .filter(|s| !s.trim().is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].assignment = value.as_deref().map(Arc::from);
    state.pending_assignment_changes.push((tab_id, value.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "assignment": value })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/parent` — stamp a spawned tab's lineage.
pub(super) fn parent<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/parent".len()];
    let value = parse_field(body_bytes, "parent_tab_id")
        .map(|s| s.chars().take(128).collect::<String>())
        .filter(|s| !s.trim().is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].parent_tab_id = value.as_deref().map(Arc::from);
    state.pending_parent_changes.push((tab_id, value.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "parent_tab_id": value })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/rehome` — set/clear a predecessor's re-home progress.
pub(super) fn rehome<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/rehome".len()];
    let value = parse_field(body_bytes, "rehome_status")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(s) = &value
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
    state.tabs[idx].rehome_status = value.as_deref().map(Arc::from);
    state.pending_rehome_changes.push((tab_id, value.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "rehome_status": value })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/{verb}` — the generic set-* card verbs (specialty/
/// orchestrator/objective OVERWRITE, current-task APPENDS, conventions OVERWRITE,
/// rounds-active toggles). Verb resolved by [`card_route_verb`].
pub(super) fn card_verb<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let (verb, json_key) = card_route_verb(p).unwrap_or(("", ""));
    let inner = &p["/tabs/by-id/".len()..p.len() - verb.len() - 1];
    // Same length cap as /assignment / /context.
    let raw = parse_field(body_bytes, json_key).map(|s| s.chars().take(2000).collect::<String>());
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
            state.tabs[idx].specialty = v.as_deref().map(Arc::from);
            CardChange::Specialty(v)
        }
        "orchestrator" => {
            let v = raw.filter(|s| !s.trim().is_empty());
            state.tabs[idx].orchestrator = v.as_deref().map(Arc::from);
            CardChange::Orchestrator(v)
        }
        "objective" => {
            let v = raw.filter(|s| !s.trim().is_empty());
            state.tabs[idx].objective = v.as_deref().map(Arc::from);
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
pub(super) fn evaluation<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
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
pub(super) fn bump_usage<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
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
