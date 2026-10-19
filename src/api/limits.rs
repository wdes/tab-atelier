// SPDX-License-Identifier: MPL-2.0

//! Per-tab and global-default cgroup resource limits (`limit`), queued to the
//! owner which applies them live.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::{TabSnapshot, parse_tab_key, resolve_tab_idx};

/// The acknowledgement a queued limit change answers.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Queued {
    queued: &'static str,
}

/// The `{overrides, clear}` pair a limit body decodes to.
///
/// Both routes take the identical body — `{"memory_max":…,"cpu_quota_percent":…,
/// "tasks_max":…}`, or `{"clear":true}` — and both validate it the same way. The
/// two handlers used to carry a copy each, so a rule added to one would have
/// silently not applied to the other.
fn parse_limits(body: &[u8]) -> Result<(crate::TabResourceLimits, bool), Failure> {
    let parsed: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
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
        return Err(Failure::problem(
            400,
            "provide memory_max / cpu_quota_percent / tasks_max, or clear:true",
        ));
    }
    if !over.memory_max_valid() {
        return Err(Failure::problem(
            400,
            "memory_max must be a byte count or K/M/G/T value (e.g. \"8G\")",
        ));
    }
    Ok((over, clear))
}

pub(super) fn set_default(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Set or clear the GLOBAL default resource limits (the CLI
    // `limit --all`). Same JSON body as the per-tab route. The owner
    // updates its live `default_tab_limits`, persists preferences.json,
    // and re-applies the cgroup to every tab (tabs without their own
    // override + all future tabs pick it up with no restart).
    let (over, clear) = parse_limits(request.body)?;
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_default_limits = Some((over, clear));
    drop(snap);
    Ok(Doc::ok(Queued {
        queued: "default-limits",
    }))
}

pub(super) fn set_tab(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Set or clear per-tab resource limits on a live tab. Body (all
    // fields optional): {"memory_max":"8G","cpu_quota_percent":250,
    // "tasks_max":512} sets those axes; {"clear":true} lifts every
    // limit back to unlimited. Accepts both /tabs/by-id/<uuid>/limits
    // and /tabs/<idx>/limits, mirroring the /catbus routes.
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/limits") else {
        return Err(Failure::problem(404, "missing tab id"));
    };
    let (over, clear) = parse_limits(request.body)?;
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_limit_changes.push((id, over, clear));
    drop(snap);
    Ok(Doc::ok(Queued { queued: "limits" }))
}
