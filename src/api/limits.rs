// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab and global-default cgroup resource limits (`limit`), queued to the
//! owner which applies them live.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn set_default<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Set or clear the GLOBAL default resource limits (the CLI
    // `limit --all`). Same JSON body as the per-tab route. The owner
    // updates its live `default_tab_limits`, persists preferences.json,
    // and re-applies the cgroup to every tab (tabs without their own
    // override + all future tabs pick it up with no restart).
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

pub(super) fn set_tab<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear per-tab resource limits on a live tab. Body (all
    // fields optional): {"memory_max":"8G","cpu_quota_percent":250,
    // "tasks_max":512} sets those axes; {"clear":true} lifts every
    // limit back to unlimited. Accepts both /tabs/by-id/<uuid>/limits
    // and /tabs/<idx>/limits, mirroring the /catbus routes.
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/limits") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
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
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_limit_changes.push((id, over, clear));
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"limits"}"#);
}
