// SPDX-License-Identifier: MPL-2.0

//! Per-tab presentation setters: background colour, tab-bar badge, and the
//! free-text agent context. Each reflects into the snapshot and queues the
//! runtime sync drained by the main loop.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::TabSnapshot;

/// The colour that was stored, or `null` when it was cleared.
///
/// A separate type per route rather than one struct with three optional fields:
/// the answer to a `bg-color` call is always about the colour, and a caller that
/// had to work out which key to read would be reading a union it never asked
/// for. Each field is `Option` because clearing is a real answer distinct from
/// "unchanged", and `null` is how the API has always spelled a clear.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ColorApplied {
    color: Option<String>,
}

/// The badge that was stored, after `sanitize_badge` normalised it.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct BadgeApplied {
    badge: Option<String>,
}

/// The context that was stored, after the length cap and the blank trim.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ContextApplied {
    context: Option<String>,
}

/// The assignment that was stored, already parsed into its parts.
///
/// Answered as the parsed object rather than the string that was sent, so a caller sending
/// `"build/"` sees that the role is empty instead of having to parse its own echo back.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AssignmentApplied {
    assignment: Option<crate::assignment::Assignment>,
}

/// The tab a `/tabs/by-id/<uuid>/<suffix>` path names.
fn tab_of<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix("/tabs/by-id/")?.strip_suffix(suffix)
}

pub(super) fn bg_color(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<ColorApplied>, Failure> {
    // Set or clear the per-tab background color override.
    // Master token only. Body: {"color": "#RRGGBB"} to set,
    // {"color": null} to clear (tab falls back to global
    // default). Validates the hex before accepting.
    let Some(inner) = tab_of(request.path, "/bg-color") else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let parsed: Option<Option<String>> = if request.body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(request.body)
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
        return Err(Failure::problem(
            400,
            "missing {\"color\": \"#RRGGBB\"} or {\"color\": null}",
        ));
    };
    // Validate hex if Some.
    if let Some(ref c) = color_opt
        && (c.len() != 7 || !c.starts_with('#') || !c[1..].chars().all(|x| x.is_ascii_hexdigit()))
    {
        return Err(Failure::problem(400, "color must be #RRGGBB"));
    }
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Reflect immediately in the snapshot so the next /output
    // poll already returns the new color; persist tick syncs
    // the runtime Tab on the next 100 ms tick.
    state.tabs[idx].bg_color = color_opt.as_deref().unwrap_or_default().into();
    state.pending_bg_color_changes.push((tab_id, color_opt.clone()));
    drop(state);
    Ok(Doc::ok(ColorApplied { color: color_opt }))
}

pub(super) fn badge(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<BadgeApplied>, Failure> {
    // Set or clear this tab's badge override — the short tag the
    // desktop draws on the tab. Body: {"badge":"KAL"} to set,
    // {"badge":null} to clear (the tab falls back to the folder rule
    // for its cwd). Master token only, like /bg-color.
    let Some(inner) = tab_of(request.path, "/badge") else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(request.body).ok();
    let Some(field) = parsed.as_ref().and_then(|v| v.get("badge")) else {
        return Err(Failure::problem(400, "missing {\"badge\": \"…\"} or {\"badge\": null}"));
    };
    let badge_opt = if field.is_null() {
        None
    } else {
        let Some(raw) = field.as_str() else {
            return Err(Failure::problem(400, "badge must be a string or null"));
        };
        Some(crate::sanitize_badge(raw).map_err(|e| Failure::problem(400, e))?)
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Reflect immediately so the next /tabs poll already shows it; the
    // persist tick syncs the runtime tab.
    state.tabs[idx].badge = badge_opt.as_deref().map(Into::into);
    state.pending_badge_changes.push((tab_id, badge_opt.clone()));
    drop(state);
    Ok(Doc::ok(BadgeApplied { badge: badge_opt }))
}

pub(super) fn context(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<ContextApplied>, Failure> {
    // Set or clear this tab's free-text context (the PR/task an
    // in-tab agent is working on). Body: {"context":"…"} to set,
    // {"context":null} or empty body to clear. RW token only.
    let Some(inner) = tab_of(request.path, "/context") else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let context_opt: Option<String> = if request.body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(request.body)
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
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].context = context_opt.as_deref().map(std::sync::Arc::from);
    state.pending_context_changes.push((tab_id, context_opt.clone()));
    drop(state);
    Ok(Doc::ok(ContextApplied { context: context_opt }))
}

pub(super) fn assignment(
    request: &Request<'_>,
    state: &Arc<Mutex<TabSnapshot>>,
) -> Result<Doc<AssignmentApplied>, Failure> {
    // Set or clear this tab's assignment — what the tab is *for*, as `[override:]phase[/role]`.
    // Body: {"assignment":"review/pr"} to set, {"assignment":null} or an empty body to clear.
    //
    // Separate from `/context`, which is a free-text note about the current PR: this is what the
    // dashboard groups by and what `dispatch --to <role>` matches, so it is parsed and stored as a
    // typed value rather than kept as a string.
    let Some(inner) = tab_of(request.path, "/assignment") else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let raw: Option<String> = if request.body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(request.body)
            .ok()
            .and_then(|v| v.get("assignment").cloned())
            .and_then(|a| {
                if a.is_null() {
                    None
                } else {
                    a.as_str().map(str::to_owned)
                }
            })
    };
    // A blank string clears, exactly as a null does: `set-assignment ""` is how a shell pipeline
    // clears a field it computed as empty, and rejecting it would make that a special case.
    let parsed = raw
        .as_deref()
        .map(crate::assignment::Assignment::parse)
        .filter(|a| !a.is_empty());
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Reflected immediately so the next `/tabs` poll already carries it; the owner loop mirrors it
    // onto the runtime tab on the next tick, the same path `/context` takes.
    state.tabs[idx].assignment.clone_from(&parsed);
    state.pending_assignment_changes.push((tab_id, parsed.clone()));
    drop(state);
    Ok(Doc::ok(AssignmentApplied { assignment: parsed }))
}
