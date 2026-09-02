// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The keystroke-input resource (`input`), refused when the tab is locked
//! (manual or schedule), queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: Vec<u8>) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/input") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) {
        // Refuse every write source — master token, share tokens, all
        // routes — when the tab is locked. `effective_locked()`
        // is the single source of truth: it covers BOTH the
        // user-toggled manual lock AND the off-hours schedule,
        // so a new gate can't accidentally honour only one.
        if crate::schedule::LockState::effective_locked(&state.tabs[idx]) {
            drop(state);
            error_json(stream, 403, "tab is locked");
            return;
        }
        info!("API: sending {} bytes of input to tab {idx}", body_bytes.len());
        let n = body_bytes.len();
        state.pending_input.push((idx, body_bytes));
        drop(state);
        let resp = serde_json::to_string(&serde_json::json!({"sent": n})).unwrap_or_default();
        respond_json(stream, 200, &resp);
    } else {
        drop(state);
        error_json(stream, 404, "tab not found");
    }
}
