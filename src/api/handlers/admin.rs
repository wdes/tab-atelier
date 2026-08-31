// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Daemon-wide admin route handlers (global limits, claude-only, relay, env,
//! token rotation, master-token reset). Master-token only (enforced upstream by
//! the auth gate — these paths aren't in any share-token allowlist). Bodies
//! moved verbatim from `handle_connection`'s match arms (behavior-preserving).

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{
    RelayConfigChange, TabSnapshot, error_json, generate_token, parse_env_body, respond_json, write_private_file,
};

/// `POST /limits/default` — set or clear the GLOBAL default resource limits.
pub(in crate::api) fn default_limits<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let clear = parsed
        .get("clear")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let over = crate::TabResourceLimits {
        memory_max: parsed.get("memory_max").and_then(|v| v.as_str()).map(str::to_owned),
        cpu_quota_percent: parsed
            .get("cpu_quota_percent")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        tasks_max: parsed.get("tasks_max").and_then(serde_json::Value::as_u64),
    };
    if !clear && over.is_empty() {
        error_json(
            stream,
            400,
            "provide memory_max / cpu_quota_percent / tasks_max, or clear:true",
        );
        return;
    }
    if !over.memory_max_valid() {
        error_json(
            stream,
            400,
            "memory_max must be a byte count or K/M/G/T value (e.g. \"8G\")",
        );
        return;
    }
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_default_limits = Some((over, clear));
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"default-limits"}"#);
}

/// `POST /claude-only` — toggle forced Claude-only mode live.
pub(in crate::api) fn claude_only<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
        error_json(stream, 400, r#"provide {"on": true|false}"#);
        return;
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_claude_only = Some(on);
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"claude-only"}"#);
}

/// `POST /relay-mode` — toggle relay mode live.
pub(in crate::api) fn relay_mode<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
        error_json(stream, 400, r#"provide {"on": true|false}"#);
        return;
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_relay_mode = Some(on);
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"relay-mode"}"#);
}

/// `GET /relay-config` — current relay config (`relay status`).
pub(in crate::api) fn relay_config_get<S: Write>(stream: &mut S) {
    let (egress, target) = (crate::relay_egress(), crate::relay_target());
    let body = serde_json::json!({
        "mode": crate::relay_mode(),
        "egress": egress,
        "target": target.map(|t| t.url),
    })
    .to_string();
    respond_json(stream, 200, &body);
}

/// `POST /relay-config` — set the relay endpoint and/or egress role.
pub(in crate::api) fn relay_config_set<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let change = RelayConfigChange {
        endpoint: parsed.get("endpoint").and_then(|v| v.as_str()).map(str::to_owned),
        egress: parsed.get("egress").and_then(serde_json::Value::as_bool),
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_relay_config = Some(change);
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"relay-config"}"#);
}

/// `GET /env` — the GLOBAL tab-env map (`env list`). Values are MASKED server-side
/// (upstream #40): a secret never leaves the daemon, only boolean-ish flags come
/// through in the clear.
pub(in crate::api) fn env_get<S: Write>(stream: &mut S) {
    let map = super::super::mask_env_map(&crate::tab_env_global());
    match serde_json::to_string(&map) {
        Ok(j) => respond_json(stream, 200, &j),
        Err(e) => error_json(stream, 500, &format!("serialize: {e}")),
    }
}

/// `GET /tabs/{id}/env` — the PER-TAB env map (`env list --tab <id>`), masked the
/// same way as `GET /env`. Mirrored from the runtime tab into the snapshot (upstream
/// #40, ported into the split).
pub(in crate::api) fn env_get_tab<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let Some((key_raw, is_uuid)) = super::super::parse_tab_key(p, "/env") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = super::super::resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let map = super::super::mask_env_map(&snap.tabs[idx].tab_env);
    drop(snap);
    match serde_json::to_string(&map) {
        Ok(j) => respond_json(stream, 200, &j),
        Err(e) => error_json(stream, 500, &format!("serialize: {e}")),
    }
}

/// `POST /env` — global env change (`env set/unset --global`).
pub(in crate::api) fn env_set<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    match parse_env_body(body_bytes) {
        Ok(change) => {
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.pending_env_changes.push(change);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"env"}"#);
        }
        Err(e) => error_json(stream, 400, &e),
    }
}

/// `POST /tabs/rotate-tokens` — revoke every per-tab share token + the global
/// dashboard share-token so all outstanding links 401.
pub(in crate::api) fn rotate_tokens<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>) {
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
    let dashboard_revoked = !state.dashboard_share_token.is_empty();
    if dashboard_revoked {
        state.dashboard_share_token = "".into();
    }
    state.invalidate_tabs();
    drop(state);
    respond_json(
        stream,
        200,
        &format!(r#"{{"revoked":{revoked},"dashboard_revoked":{dashboard_revoked}}}"#),
    );
}

/// `POST /master-token/reset` — hot-swap the master API token (persist + publish).
pub(in crate::api) fn master_token_reset<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>) {
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
