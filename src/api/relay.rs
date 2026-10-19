// SPDX-License-Identifier: MPL-2.0

//! The relay control resource: live `relay on|off` toggle and the
//! endpoint/egress config read/write, all queued to the owner.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::{RelayConfigChange, TabSnapshot};

/// The acknowledgement a queued relay change answers.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Queued {
    queued: &'static str,
}

/// The relay's current configuration, as `relay status` shows it.
///
/// `target` is the URL of the configured remote, and it is `None` rather than
/// `""` when there is none — a caller deciding whether to route through a remote
/// should not have to know that an empty string means absent.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Config {
    mode: bool,
    egress: bool,
    target: Option<String>,
}

pub(super) fn mode(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Toggle relay mode live (the CLI `relay on|off`). Body:
    // {"on": true|false}. The owner mirrors it onto RELAY_MODE + its
    // struct field and persists; claude tabs spawned after route their
    // Anthropic calls through the configured remote.
    let parsed: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
        return Err(Failure::problem(400, r#"provide {"on": true|false}"#));
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_relay_mode = Some(on);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "relay-mode" }))
}

pub(super) fn config_get() -> Doc<Config> {
    // Current relay config (the CLI `relay status`).
    Doc::ok(Config {
        mode: crate::relay_mode(),
        egress: crate::relay_egress(),
        target: crate::relay_target().map(|t| t.url),
    })
}

pub(super) fn config_set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Set the relay endpoint and/or egress role (`relay via` / `relay
    // egress`). Body: {"endpoint":"<label|id|"">","egress":bool} — any
    // subset. The owner resolves the endpoint, persists, and re-installs.
    let parsed: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let change = RelayConfigChange {
        endpoint: parsed.get("endpoint").and_then(|v| v.as_str()).map(str::to_owned),
        egress: parsed.get("egress").and_then(serde_json::Value::as_bool),
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_relay_config = Some(change);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "relay-config" }))
}
