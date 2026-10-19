// SPDX-License-Identifier: MPL-2.0

//! The per-tab `meta` label resource (`set-meta`): validated, capped key/value
//! changes queued onto the tab's durable meta map.
//!
//! The first route written against the shared HTTP contract rather than the
//! daemon's own writers. It takes a [`Request`] and answers a [`Reply`], so the
//! same function is callable from this listener and from a Rocket route — and
//! its refusals already have the shape every other server in the workspace
//! produces.

use std::sync::{Arc, Mutex};

use tab_atelier_api::{Reply, Request, problem};

use super::{MetaChange, TabSnapshot, parse_tab_key, resolve_tab_idx};

/// Queue one label change on one tab.
pub(super) fn set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Reply {
    // Set or clear one free-form durable label on a tab (`set-meta`).
    // Body: {"key":"role","value":"reviewer"} to set,
    // {"key":"role","value":null} to remove. Keys/values are validated
    // by `crate::sanitize_meta`; the map is capped at META_MAX_KEYS so
    // a chatty producer can't grow tabs.json without bound.
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/meta") else {
        return problem(404, "missing tab id");
    };
    let parsed: serde_json::Value = serde_json::from_slice(request.body).unwrap_or(serde_json::Value::Null);
    let Some(key) = parsed.get("key").and_then(|v| v.as_str()) else {
        return problem(400, "expected {\"key\":\"…\",\"value\":\"…\"|null}");
    };
    // A null (or absent) value removes the key; anything else must
    // validate as a value.
    let raw_value = parsed.get("value").and_then(|v| v.as_str());
    let (key, value) = match raw_value {
        Some(v) => match crate::sanitize_meta(key, v) {
            Ok((k, v)) => (k, Some(v)),
            Err(e) => return problem(400, &e),
        },
        // Validate the key alone by round-tripping a dummy value.
        None => match crate::sanitize_meta(key, "x") {
            Ok((k, _)) => (k, None),
            Err(e) => return problem(400, &e),
        },
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return problem(404, "tab not found");
    };
    let full =
        value.is_some() && snap.tabs[idx].meta.len() >= crate::META_MAX_KEYS && !snap.tabs[idx].meta.contains_key(&key);
    if full {
        return problem(400, &format!("meta is full ({} keys)", crate::META_MAX_KEYS));
    }
    let tab_id = snap.tabs[idx].id.to_string();
    snap.pending_meta_changes.push(MetaChange { tab_id, key, value });
    // Released before the reply is built: the lock guards the snapshot, not
    // the serialization, and clippy is right that holding it longer invites a
    // deadlock the day a reply reads state.
    drop(snap);
    tab_atelier_api::json(200, r#"{"queued":"meta"}"#)
}
