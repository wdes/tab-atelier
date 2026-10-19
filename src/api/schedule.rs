// SPDX-License-Identifier: MPL-2.0

//! The per-tab off-hours auto-lock schedule resource, validated through
//! `TabSchedule::new` and queued to the owner.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::TabSnapshot;

/// The schedule as stored, or a bare `{"rule": null}` when it was cleared.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Stored {
    rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tz: Option<String>,
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Stored>, Failure> {
    // Set or clear the off-hours auto-lock schedule. Master
    // token only — same gate as /lock and /bg-color (the
    // share-token route table refuses everything past
    // /output|/stream|/input|/files).
    //
    // Body: `{"rule": "Mo-Fr 09:00-18:00", "tz": "Europe/Paris"}`
    // to set; `{"rule": null}` or `{}` to clear (tab goes
    // back to 24/7 unless still manually locked).
    //
    // Validation runs through `TabSchedule::new`, which
    // rejects empty fields, unknown tzs, and unparseable
    // rules. We surface the parser's own error string so the
    // CLI / GUI can show the user exactly what failed.
    #[derive(serde::Deserialize)]
    struct Body {
        rule: Option<String>,
        tz: Option<String>,
    }
    let Some(inner) = request
        .path
        .strip_prefix("/tabs/by-id/")
        .and_then(|rest| rest.strip_suffix("/schedule"))
    else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let parsed: Option<Body> = if request.body.is_empty() {
        Some(Body { rule: None, tz: None })
    } else {
        serde_json::from_slice::<Body>(request.body).ok()
    };
    let Some(body) = parsed else {
        return Err(Failure::problem(400, "invalid JSON body"));
    };
    let schedule_opt: Option<crate::schedule::TabSchedule> = match (body.rule.as_deref(), body.tz.as_deref()) {
        (None | Some(""), _) => None,
        (Some(rule), Some(tz)) => {
            Some(crate::schedule::TabSchedule::new(rule, tz).map_err(|e| Failure::problem(400, format!("{e}")))?)
        }
        (Some(_), None) => {
            return Err(Failure::problem(400, "tz is required when rule is set"));
        }
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Mirror immediately in the snapshot so the next /output
    // poll already returns the new locked state via
    // `effective_locked`; persist tick mirrors onto the runtime
    // Tab on the next 100 ms tick.
    state.tabs[idx].schedule.clone_from(&schedule_opt);
    state.pending_schedule_changes.push((tab_id, schedule_opt.clone()));
    drop(state);
    let stored = schedule_opt
        .as_ref()
        .map_or(Stored { rule: None, tz: None }, |s| Stored {
            rule: Some(s.rule.clone()),
            tz: Some(s.tz.clone()),
        });
    Ok(Doc::ok(stored))
}
