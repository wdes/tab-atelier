// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-tab lock toggle (`lock`/`unlock`), refused outside a schedule's
//! open windows, queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Flip the per-tab lock from the CLI / API. Master token
    // only (share-token gate above does not allow `/lock`).
    // ?on=1/0 takes precedence; absent → toggle.
    let inner = &p["/tabs/by-id/".len()..p.len() - "/lock".len()];
    // Pull `?on=` from the original path. `path` here is the
    // already-stripped form; the original is `raw_path` but
    // it's already been moved by this point — re-derive from
    // the body for the body-driven form, or accept the URL
    // form by looking at the request line earlier captures.
    // Simplest: accept `{"on": true|false}` in the JSON body.
    let on_body: Option<bool> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("on").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let current_locked = state.tabs[idx].locked;
    let new_val = on_body.unwrap_or(!current_locked);
    // Manual unlock OUTSIDE the schedule's open windows is
    // refused — the schedule is the boundary, not a polite
    // suggestion. The user can still lock during open hours
    // (manual lock beats schedule open). If they want to
    // unlock outside hours, they remove the schedule first.
    //
    // Probe the post-unlock state — pass `false` to the
    // helper to simulate "what would the lock_reason be
    // after the unlock?" If the answer is still
    // schedule-driven, refuse. Routes through the same
    // `lock_reason` helper as every other gate so a future
    // change to the rule is automatically picked up here.
    if !new_val && crate::schedule::lock_reason(false, state.tabs[idx].schedule.as_ref()) == Some("schedule") {
        drop(state);
        error_json(stream, 423, "schedule is closed");
        return;
    }
    state.tabs[idx].locked = new_val;
    state.pending_lock_changes.push((tab_id, new_val));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"locked": new_val})).unwrap_or_default();
    respond_json(stream, 200, &body);
}
