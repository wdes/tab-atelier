// SPDX-License-Identifier: MPL-2.0

//! Token administration: revoke every tab's share tokens, and hot-swap the
//! master API token. Master token only.

use super::{
    TabSnapshot, error_json, generate_token, parse_tab_key, resolve_tab_idx, respond_json, write_private_file,
};
use std::io::Write;
use std::sync::{Arc, Mutex};

/// `POST /tabs/by-id/{id}/share` — return a tab's RW share token, minting one
/// if it has none.
///
/// The share token is the credential a share link carries: it authorises the
/// viewer and the input path for **one** tab, it is rotatable with
/// `POST /tabs/rotate-tokens`, and it is not the master token. Minting it here
/// lets a caller that is not the GUI — the Kiosk, say — build a terminal link
/// without ever handling the master token. Master token only: a share token
/// must not be able to mint another.
pub(super) fn mint_share<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, path: &str) {
    let Some((key, is_uuid)) = parse_tab_key(path, "/share") else {
        error_json(stream, 400, "bad tab path");
        return;
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key, is_uuid) else {
        error_json(stream, 404, "tab not found");
        return;
    };
    let token = if state.tabs[idx].share_token_rw.is_empty() {
        let token = crate::mint_share_token();
        // Captured before the mutable write: the id is read from the same
        // `state` that `pending_share_mints` borrows.
        let tab_id = state.tabs[idx].id.to_string();
        state.tabs[idx].share_token_rw = token.clone().into();
        // Queue it for the owner loop: the snapshot is rebuilt from the runtime
        // Tab each tick, so without this the token would be gone in a moment.
        state.pending_share_mints.push((tab_id, token.clone()));
        state.invalidate_tabs();
        token
    } else {
        state.tabs[idx].share_token_rw.to_string()
    };
    respond_json(stream, 200, &format!(r#"{{"rw":"{token}"}}"#));
}

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
