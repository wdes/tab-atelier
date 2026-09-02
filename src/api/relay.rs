// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The relay control resource: live `relay on|off` toggle and the
//! endpoint/egress config read/write, all queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{RelayConfigChange, TabSnapshot, error_json, respond_json};

pub(super) fn mode<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Toggle relay mode live (the CLI `relay on|off`). Body:
    // {"on": true|false}. The owner mirrors it onto RELAY_MODE + its
    // struct field and persists; claude tabs spawned after route their
    // Anthropic calls through the configured remote.
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

pub(super) fn config_get<W: Write>(stream: &mut W) {
    // Current relay config (the CLI `relay status`).
    let (egress, target) = (crate::relay_egress(), crate::relay_target());
    let body = serde_json::json!({
        "mode": crate::relay_mode(),
        "egress": egress,
        "target": target.map(|t| t.url),
    })
    .to_string();
    respond_json(stream, 200, &body);
}

pub(super) fn config_set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Set the relay endpoint and/or egress role (`relay via` / `relay
    // egress`). Body: {"endpoint":"<label|id|"">","egress":bool} — any
    // subset. The owner resolves the endpoint, persists, and re-installs.
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
