// SPDX-License-Identifier: MPL-2.0

//! The tab grid-size pin resource (`resize`), queued to the owner.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};
// The derive emits `schemars::` paths, so the name itself has to be in scope.
use tab_atelier_api::schemars;

use super::{TabSnapshot, parse_tab_key, resolve_tab_idx};

/// What a resize answers: the acknowledgement every queued route gives.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Queued {
    queued: &'static str,
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Pin (or clear) a tab's fixed grid size (the CLI `resize`). Body:
    // {"cols":N,"rows":M} pins to that size (both >= 2 / >= 1), or
    // {"clear":true} un-pins it back to window-driven sizing. Accepts
    // /tabs/by-id/<uuid>/resize and /tabs/<idx>/resize.
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/resize") else {
        return Err(Failure::problem(404, "missing tab id"));
    };
    let parsed: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let clear = parsed
        .get("clear")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let dims = if clear {
        None
    } else {
        let cols = parsed
            .get("cols")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok());
        let rows = parsed
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok());
        match (cols, rows) {
            (Some(c), Some(r)) if c >= 2 && r >= 1 => Some((c, r)),
            _ => {
                return Err(Failure::problem(
                    400,
                    "provide cols (>=2) and rows (>=1), or clear:true",
                ));
            }
        }
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_resizes.push((id, dims));
    drop(snap);
    Ok(Doc::ok(Queued { queued: "resize" }))
}
