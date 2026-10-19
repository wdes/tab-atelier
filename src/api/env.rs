// SPDX-License-Identifier: MPL-2.0

//! The `env`-var HTTP resource: masked `env list` (global + per-tab) and the
//! queued `env set/unset` changes drained by the main loop.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tab_atelier_api::{Doc, Failure, Request};

use super::{EnvChange, TabSnapshot, parse_tab_key, resolve_tab_idx};

/// The masked env map, exactly as this route has always sent it.
///
/// A bare `{KEY: VALUE}` object, NOT a wrapper with a name around it: `env list`
/// and every script that parses it read the map at the top level, and dressing
/// it up while migrating the writer would be a wire change disguised as a
/// refactor. Masked on the server (see [`mask_env_value`]) is the part that
/// matters — a secret never leaves the daemon.
type EnvMap = BTreeMap<String, String>;

pub(super) fn list_global() -> Doc<EnvMap> {
    // The GLOBAL tab-env map (the CLI `env list`). Values are MASKED
    // server-side (see `mask_env_value`): a secret never leaves the
    // daemon; only boolean-ish flags (0/1/true/false) come through in
    // the clear. Keeps the wire clean of API keys / tokens.
    Doc::ok(mask_env_map(&crate::tab_env_global()))
}

pub(super) fn list_tab(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<EnvMap>, Failure> {
    // Per-tab env map (`env list --tab <id>`), values masked the same way
    // as `GET /env` above. Mirrored from the runtime tab into the
    // snapshot, so it reflects queued changes as soon as the next
    // snapshot rebuild lands (same cadence as `/tabs`).
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/env") else {
        return Err(Failure::problem(404, "missing tab id"));
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let vars = mask_env_map(&snap.tabs[idx].tab_env);
    drop(snap);
    Ok(Doc::ok(vars))
}

pub(super) fn set_global(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Global env change (`env set/unset --global`). Body:
    // {"set":{"K":"V"},"unset":["K"],"respawn":bool}.
    let change = parse_env_body(request.body).map_err(|e| Failure::problem(400, e))?;
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_env_changes.push(change);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "env" }))
}

pub(super) fn set_tab(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Per-tab env change (`env set/unset --tab <id>`).
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/env") else {
        return Err(Failure::problem(404, "missing tab id"));
    };
    let parsed = parse_env_body(request.body);
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let id = snap.tabs[idx].id.to_string();
    let mut change = parsed.map_err(|e| Failure::problem(400, e))?;
    change.tab = Some(id);
    snap.pending_env_changes.push(change);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "env" }))
}

/// The acknowledgement a queued change answers.
#[derive(Debug, serde::Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(super) struct Queued {
    queued: &'static str,
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
