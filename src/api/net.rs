// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-tab full-airgap resource (`net-off` / `net-on`): a bubblewrap
//! net-namespace jail toggle, queued to the owner (the shell respawns to apply).

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Turn the tab's internet off / on (bubblewrap net-namespace
    // jail). Master token only (share-token gate above does not
    // allow `/net`). Body `{"disabled": true|false}`; absent →
    // toggle. The shell respawns to apply, so the change isn't
    // instantaneous — the runtime tab picks it up next tick.
    let inner = &p["/tabs/by-id/".len()..p.len() - "/net".len()];
    let disabled_body: Option<bool> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("disabled").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let new_val = disabled_body.unwrap_or(!state.tabs[idx].net_disabled);
    // Refuse turning net OFF when bubblewrap isn't installed —
    // there's no way to build the netns jail, and silently
    // leaving the net on would be a lie. Turning net back ON is
    // always allowed (no bwrap needed to un-jail).
    if new_val && !crate::bwrap_available() {
        drop(state);
        error_json(stream, 412, "bubblewrap (bwrap) is not installed");
        return;
    }
    state.tabs[idx].net_disabled = new_val;
    state.pending_net_changes.push((tab_id, new_val));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"net_disabled": new_val})).unwrap_or_default();
    respond_json(stream, 200, &body);
}
