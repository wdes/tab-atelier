// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab presentation setters: background colour, tab-bar badge, and the
//! free-text agent context. Each reflects into the snapshot and queues the
//! runtime sync drained by the main loop.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn bg_color<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear the per-tab background color override.
    // Master token only. Body: {"color": "#RRGGBB"} to set,
    // {"color": null} to clear (tab falls back to global
    // default). Validates the hex before accepting.
    let inner = &p["/tabs/by-id/".len()..p.len() - "/bg-color".len()];
    let parsed: Option<Option<String>> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| {
                let c = v.get("color")?;
                if c.is_null() {
                    Some(None)
                } else {
                    c.as_str().map(|s| Some(s.to_string()))
                }
            })
    };
    let Some(color_opt) = parsed else {
        error_json(stream, 400, "missing {\"color\": \"#RRGGBB\"} or {\"color\": null}");
        return;
    };
    // Validate hex if Some.
    if let Some(ref c) = color_opt
        && (c.len() != 7 || !c.starts_with('#') || !c[1..].chars().all(|x| x.is_ascii_hexdigit()))
    {
        error_json(stream, 400, "color must be #RRGGBB");
        return;
    }
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Reflect immediately in the snapshot so the next /output
    // poll already returns the new color; persist tick syncs
    // the runtime Tab on the next 100 ms tick.
    state.tabs[idx].bg_color = color_opt.as_deref().unwrap_or_default().into();
    state.pending_bg_color_changes.push((tab_id, color_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({
        "color": color_opt
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}

pub(super) fn badge<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear this tab's badge override — the short tag the
    // desktop draws on the tab. Body: {"badge":"KAL"} to set,
    // {"badge":null} to clear (the tab falls back to the folder rule
    // for its cwd). Master token only, like /bg-color.
    let inner = &p["/tabs/by-id/".len()..p.len() - "/badge".len()];
    let parsed = serde_json::from_slice::<serde_json::Value>(body_bytes).ok();
    let Some(field) = parsed.as_ref().and_then(|v| v.get("badge")) else {
        error_json(stream, 400, "missing {\"badge\": \"…\"} or {\"badge\": null}");
        return;
    };
    let badge_opt = if field.is_null() {
        None
    } else {
        let Some(raw) = field.as_str() else {
            error_json(stream, 400, "badge must be a string or null");
            return;
        };
        match crate::sanitize_badge(raw) {
            Ok(b) => Some(b),
            Err(e) => {
                error_json(stream, 400, &e);
                return;
            }
        }
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Reflect immediately so the next /tabs poll already shows it; the
    // persist tick syncs the runtime tab.
    state.tabs[idx].badge = badge_opt.as_deref().map(Into::into);
    state.pending_badge_changes.push((tab_id, badge_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "badge": badge_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

pub(super) fn context<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear this tab's free-text context (the PR/task an
    // in-tab agent is working on). Body: {"context":"…"} to set,
    // {"context":null} or empty body to clear. RW token only.
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
    // Cap length so a runaway agent can't bloat the snapshot /
    // tooltip; trim whitespace-only to a clear.
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
