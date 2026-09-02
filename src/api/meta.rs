// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-tab `meta` label resource (`set-meta`): validated, capped key/value
//! changes queued onto the tab's durable meta map.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{MetaChange, TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear one free-form durable label on a tab (`set-meta`).
    // Body: {"key":"role","value":"reviewer"} to set,
    // {"key":"role","value":null} to remove. Keys/values are validated
    // by `crate::sanitize_meta`; the map is capped at META_MAX_KEYS so
    // a chatty producer can't grow tabs.json without bound.
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/meta") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let parsed: serde_json::Value = serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
    let Some(key) = parsed.get("key").and_then(|v| v.as_str()) else {
        error_json(stream, 400, "expected {\"key\":\"…\",\"value\":\"…\"|null}");
        return;
    };
    // A null (or absent) value removes the key; anything else must
    // validate as a value.
    let raw_value = parsed.get("value").and_then(|v| v.as_str());
    let (key, value) = match raw_value {
        Some(v) => match crate::sanitize_meta(key, v) {
            Ok((k, v)) => (k, Some(v)),
            Err(e) => {
                error_json(stream, 400, &e);
                return;
            }
        },
        // Validate the key alone by round-tripping a dummy value.
        None => match crate::sanitize_meta(key, "x") {
            Ok((k, _)) => (k, None),
            Err(e) => {
                error_json(stream, 400, &e);
                return;
            }
        },
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let full = value.is_some()
        && snap.tabs[idx].meta.len() >= crate::META_MAX_KEYS
        && !snap.tabs[idx].meta.contains_key(&key);
    if full {
        drop(snap);
        error_json(stream, 400, &format!("meta is full ({} keys)", crate::META_MAX_KEYS));
        return;
    }
    let tab_id = snap.tabs[idx].id.to_string();
    snap.pending_meta_changes.push(MetaChange { tab_id, key, value });
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"meta"}"#);
}
