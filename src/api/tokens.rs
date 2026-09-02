// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Token administration: revoke every tab's share tokens, and hot-swap the
//! master API token. Master token only.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, generate_token, respond_json, write_private_file};

pub(super) fn rotate<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>) {
    // Revoke every tab's per-tab share tokens so all outstanding
    // share links 401. Cleared on the snapshot immediately
    // (instant effect) and queued so the owner loop clears the
    // runtime Tab + persists; a fresh token is minted on the next
    // "Remote control" / `share-link`. Master token only — this
    // path isn't in the share-token allowlist, so a share token
    // never authorises here.
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut revoked = 0usize;
    for t in &mut state.tabs {
        if t.share_token_rw.is_empty() && t.share_token_ro.is_empty() {
            continue;
        }
        t.share_token_rw = "".into();
        t.share_token_ro = "".into();
        revoked += 1;
    }
    let ids: Vec<String> = state.tabs.iter().map(|t| t.id.to_string()).collect();
    state.pending_token_rotations.extend(ids);
    state.invalidate_tabs();
    drop(state);
    respond_json(stream, 200, &format!(r#"{{"revoked":{revoked}}}"#));
}

pub(super) fn reset_master<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>) {
    // Hot-swap the master API token: generate a fresh one, persist
    // it to api.token (so `tab-atelier token` and saved configs
    // re-read it), and publish it onto the snapshot the auth gate
    // validates against. Every link / client carrying the OLD
    // master token 401s on its next request. Master token only
    // (this path isn't in the share-token allowlist).
    let new = generate_token();
    let dir = crate::platform::state_base_dir().join(crate::APP_DIR);
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = write_private_file(&dir.join("api.token"), new.as_bytes()) {
        error_json(stream, 500, &format!("could not persist token: {e}"));
        return;
    }
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .master_token
        .clone_from(&new);
    respond_json(stream, 200, &format!(r#"{{"token":"{new}"}}"#));
}
