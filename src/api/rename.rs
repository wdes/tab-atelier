// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tab rename resource (`POST /tabs/<idx>/rename`), queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let idx_str = &p["/tabs/".len()..p.len() - "/rename".len()];
    if let Ok(idx) = idx_str.parse::<usize>() {
        let body = body_bytes;
        let new_name = serde_json::from_slice::<serde_json::Value>(body).map_or_else(
            |_| String::from_utf8_lossy(body).trim().to_string(),
            |v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
        );
        if new_name.is_empty() {
            error_json(stream, 400, "missing or empty name");
            return;
        }
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if idx < state.tabs.len() {
            info!("API: renaming tab {idx} to {new_name}");
            state.pending_renames.push((idx, new_name.clone()));
            drop(state);
            let body =
                serde_json::to_string(&serde_json::json!({"renamed": idx, "name": new_name})).unwrap_or_default();
            respond_json(stream, 200, &body);
        } else {
            error_json(stream, 404, "tab index out of range");
        }
    } else {
        error_json(stream, 404, "invalid tab index");
    }
}
