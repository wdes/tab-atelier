// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tab grid-size pin resource (`resize`), queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Pin (or clear) a tab's fixed grid size (the CLI `resize`). Body:
    // {"cols":N,"rows":M} pins to that size (both >= 2 / >= 1), or
    // {"clear":true} un-pins it back to window-driven sizing. Accepts
    // /tabs/by-id/<uuid>/resize and /tabs/<idx>/resize.
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/resize") else {
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
                error_json(stream, 400, "provide cols (>=2) and rows (>=1), or clear:true");
                return;
            }
        }
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_resizes.push((id, dims));
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"resize"}"#);
}
