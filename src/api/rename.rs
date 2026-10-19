// SPDX-License-Identifier: MPL-2.0

//! The tab rename resource (`POST /tabs/<idx>/rename`), queued to the owner.

use std::sync::{Arc, Mutex};

use log::info;
use serde::Serialize;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};
// The derive emits `schemars::` paths, so the name itself has to be in scope.
use tab_atelier_api::schemars;

use super::TabSnapshot;

/// What a rename answers: the index it applied to and the name it took.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Renamed {
    renamed: usize,
    name: String,
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Renamed>, Failure> {
    let path = request.path;
    let Some(idx_str) = path
        .strip_prefix("/tabs/")
        .and_then(|rest| rest.strip_suffix("/rename"))
    else {
        return Err(Failure::problem(404, "invalid tab index"));
    };
    let Ok(idx) = idx_str.parse::<usize>() else {
        return Err(Failure::problem(404, "invalid tab index"));
    };
    // A JSON body is `{"name": …}`; a bare one is the name itself, which is what
    // a `curl -d` with no quoting sends.
    let new_name = serde_json::from_slice::<serde_json::Value>(request.body).map_or_else(
        |_| String::from_utf8_lossy(request.body).trim().to_string(),
        |v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
    );
    if new_name.is_empty() {
        return Err(Failure::problem(400, "missing or empty name"));
    }
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if idx >= state.tabs.len() {
        return Err(Failure::problem(404, "tab index out of range"));
    }
    info!("API: renaming tab {idx} to {new_name}");
    state.pending_renames.push((idx, new_name.clone()));
    drop(state);
    Ok(Doc::ok(Renamed {
        renamed: idx,
        name: new_name,
    }))
}
