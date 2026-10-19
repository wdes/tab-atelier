// SPDX-License-Identifier: MPL-2.0

//! The keystroke-input resource (`input`), refused when the tab is locked
//! (manual or schedule), queued to the owner.

use std::sync::{Arc, Mutex};

use log::info;
use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::{TabSnapshot, parse_tab_key, resolve_tab_idx};

/// How many bytes were queued for the tab.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Sent {
    sent: usize,
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Sent>, Failure> {
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/input") else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    // Refuse every write source — master token, share tokens, all
    // routes — when the tab is locked. `effective_locked()`
    // is the single source of truth: it covers BOTH the
    // user-toggled manual lock AND the off-hours schedule,
    // so a new gate can't accidentally honour only one.
    if crate::schedule::LockState::effective_locked(&state.tabs[idx]) {
        return Err(Failure::problem(403, "tab is locked"));
    }
    let n = request.body.len();
    info!("API: sending {n} bytes of input to tab {idx}");
    // The queue owns its bytes — the owner loop drains it after this request
    // has gone out of scope — so this is the one handler that copies the body
    // rather than reading it in place.
    state.pending_input.push((idx, request.body.to_vec()));
    drop(state);
    Ok(Doc::ok(Sent { sent: n }))
}
