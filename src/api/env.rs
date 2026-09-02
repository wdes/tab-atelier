// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `env`-var HTTP resource: masked `env list` (global + per-tab) and the
//! queued `env set/unset` changes drained by the main loop.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{EnvChange, TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn list_global<W: Write>(stream: &mut W) {
    // The GLOBAL tab-env map (the CLI `env list`). Values are MASKED
    // server-side (see `mask_env_value`): a secret never leaves the
    // daemon; only boolean-ish flags (0/1/true/false) come through in
    // the clear. Keeps the wire clean of API keys / tokens.
    let map = mask_env_map(&crate::tab_env_global());
    match serde_json::to_string(&map) {
        Ok(j) => respond_json(stream, 200, &j),
        Err(e) => error_json(stream, 500, &format!("serialize: {e}")),
    }
}

pub(super) fn list_tab<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    // Per-tab env map (`env list --tab <id>`), values masked the same way
    // as `GET /env` above. Mirrored from the runtime tab into the
    // snapshot, so it reflects queued changes as soon as the next
    // snapshot rebuild lands (same cadence as `/tabs`).
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/env") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let map = mask_env_map(&snap.tabs[idx].tab_env);
    drop(snap);
    match serde_json::to_string(&map) {
        Ok(j) => respond_json(stream, 200, &j),
        Err(e) => error_json(stream, 500, &format!("serialize: {e}")),
    }
}

pub(super) fn set_global<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Global env change (`env set/unset --global`). Body:
    // {"set":{"K":"V"},"unset":["K"],"respawn":bool}.
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

pub(super) fn set_tab<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Per-tab env change (`env set/unset --tab <id>`).
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/env") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let parsed = parse_env_body(body_bytes);
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    match parsed {
        Ok(mut change) => {
            change.tab = Some(id);
            snap.pending_env_changes.push(change);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"env"}"#);
        }
        Err(e) => {
            drop(snap);
            error_json(stream, 400, &e);
        }
    }
}

/// Mask a single env value for the `env list` API.
///
/// Boolean-ish flags (`0`/`1`/`true`/`false`, the last two case-insensitively)
/// are not secrets and pass through in the clear so an operator can eyeball a
/// feature toggle; everything else — API keys, tokens, connection strings — is
/// replaced by `******` so the real value never leaves the daemon.
fn mask_env_value(v: &str) -> &str {
    if matches!(v, "0" | "1") || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false") {
        v
    } else {
        "******"
    }
}

/// Apply [`mask_env_value`] across an env map, preserving key order (`BTreeMap`
/// ⇒ sorted). The returned map is safe to serialize over the wire.
fn mask_env_map(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.clone(), mask_env_value(v).to_string()))
        .collect()
}

/// Parse an env-change body: `{"set":{"K":"V"},"unset":["K"],"respawn":bool}`.
/// Returns an [`EnvChange`] with `tab: None`; the caller sets the tab.
fn parse_env_body(body: &[u8]) -> Result<EnvChange, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let set = v
        .get("set")
        .and_then(serde_json::Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let unset = v
        .get("unset")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    Ok(EnvChange { tab: None, set, unset })
}

#[cfg(test)]
mod tests {
    #[test]
    fn mask_env_value_covers_flags_and_secrets() {
        assert_eq!(super::mask_env_value("0"), "0");
        assert_eq!(super::mask_env_value("1"), "1");
        assert_eq!(super::mask_env_value("true"), "true");
        assert_eq!(super::mask_env_value("True"), "True");
        assert_eq!(super::mask_env_value("FALSE"), "FALSE");
        assert_eq!(super::mask_env_value("sk-abc123"), "******");
        assert_eq!(super::mask_env_value("2"), "******");
        assert_eq!(super::mask_env_value(""), "******");
        assert_eq!(super::mask_env_value("truthy"), "******");
    }
}
