// SPDX-License-Identifier: MPL-2.0

//! The forced Claude-only mode toggle (`POST /claude-only`), queued to the
//! owner which mirrors it onto `CLAUDE_ONLY` and persists.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};
// The derive emits `schemars::` paths, so the name itself has to be in scope.
use tab_atelier_api::schemars;

use super::TabSnapshot;

/// The acknowledgement a queued toggle answers.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Queued {
    queued: &'static str,
}

pub(super) fn set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Toggle forced Claude-only mode live (the CLI `claude-only on|off`).
    // Body: {"on": true|false}. The owner mirrors it onto CLAUDE_ONLY +
    // its struct field and persists, so new tabs launch claude (auto
    // mode) or a shell with no restart.
    let parsed: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
        return Err(Failure::problem(400, r#"provide {"on": true|false}"#));
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_claude_only = Some(on);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "claude-only" }))
}
