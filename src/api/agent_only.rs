// SPDX-License-Identifier: MPL-2.0

//! The forced agent-only mode switch (`POST /agent-only`), queued to the
//! owner which mirrors it onto the process-global and persists.

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

const BODY_HINT: &str = r#"provide {"agent": "claude"|"catbus"|"off"}"#;

pub(super) fn set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Queued>, Failure> {
    // Switch forced agent-only mode live (the CLI `agent-only …`).
    // Body: {"agent": "claude"|"catbus"|"off"}. The owner mirrors it onto the
    // global + its struct field and persists, so new tabs launch that agent
    // or a shell with no restart.
    let parsed: serde_json::Value =
        serde_json::from_slice(request.body).map_err(|e| Failure::problem(400, format!("invalid JSON body: {e}")))?;
    let agent = match parsed.get("agent").and_then(serde_json::Value::as_str) {
        Some("off") => crate::AgentOnlyChange::Off,
        Some(word) => match crate::AgentOnly::parse(word) {
            Some(agent) => crate::AgentOnlyChange::On(agent),
            None => return Err(Failure::problem(400, BODY_HINT)),
        },
        None => return Err(Failure::problem(400, BODY_HINT)),
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_agent_only = Some(agent);
    drop(snap);
    Ok(Doc::ok(Queued { queued: "agent-only" }))
}
