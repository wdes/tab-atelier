// SPDX-License-Identifier: MPL-2.0

//! The per-tab lock toggle (`lock`/`unlock`), refused outside a schedule's
//! open windows, queued to the owner.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};
// The derive emits `schemars::` paths, so the name itself has to be in scope.
use tab_atelier_api::schemars;

use super::TabSnapshot;

/// The new lock state, echoed so a caller can see the toggle it did not name.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Locked {
    locked: bool,
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Locked>, Failure> {
    // Flip the per-tab lock from the CLI / API. Master token only (the
    // share-token gate above does not allow `/lock`).
    //
    // `{"on": true|false}` in the body decides; absent toggles. The body is the
    // only form: the `?on=` query spelling this used to document never worked —
    // the query was already consumed into typed variables by the time routing
    // reached here, and reading it back from a path that had been stripped was
    // not possible. A caller that sends no body gets the toggle, which is what
    // `tab-atelier lock` sends.
    let Some(inner) = request
        .path
        .strip_prefix("/tabs/by-id/")
        .and_then(|rest| rest.strip_suffix("/lock"))
    else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let on_body: Option<bool> = if request.body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(request.body)
            .ok()
            .and_then(|v| v.get("on").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    let current_locked = state.tabs[idx].locked;
    let new_val = on_body.unwrap_or(!current_locked);
    // Manual unlock OUTSIDE the schedule's open windows is refused — the
    // schedule is the boundary, not a polite suggestion. The user can still
    // lock during open hours (manual lock beats schedule open). To unlock
    // outside hours they remove the schedule first.
    //
    // Probe the post-unlock state — pass `false` to the helper to ask "what
    // would `lock_reason` be after the unlock?" If the answer is still
    // schedule-driven, refuse. Going through the same `lock_reason` helper as
    // every other gate means a future change to the rule is picked up here for
    // free.
    if !new_val && crate::schedule::lock_reason(false, state.tabs[idx].schedule.as_ref()) == Some("schedule") {
        return Err(Failure::problem(423, "schedule is closed"));
    }
    state.tabs[idx].locked = new_val;
    state.pending_lock_changes.push((tab_id, new_val));
    drop(state);
    Ok(Doc::ok(Locked { locked: new_val }))
}
