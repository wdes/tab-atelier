// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `GET /fleet` — the fleet as a graph, for rendering.
//!
//! A thin join: the board says what work exists and who was awarded it, the
//! lease registry says what is actually held right now, the tab list says
//! which agents are alive here, and the federation directory says who our
//! peers are. [`crate::fleet::build`] does the joining; this gathers.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, respond_json, respond_json_cors};

pub(super) fn get<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, from_loopback: bool) {
    let tabs: Vec<crate::fleet::AgentTab> = {
        let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        snap.tabs
            .iter()
            .map(|t| crate::fleet::AgentTab {
                name: t.name.to_string(),
                uuid: t.id.to_string(),
                state: t.agent_state.as_ref().map(|a| {
                    match a.state {
                        crate::AgentState::Thinking => "thinking",
                        crate::AgentState::Waiting => "waiting",
                        crate::AgentState::Error => "error",
                    }
                    .to_owned()
                }),
            })
            .collect()
    };
    let now = crate::unix_millis();
    let board = crate::cli::tasks::fold_tasks(&crate::cli::team::read_blackboard());
    let claims = crate::claims::with_registry(|r| r.active(now));
    let graph = crate::fleet::build(
        &board,
        &claims,
        &tabs,
        &crate::cli::team::origin_id(),
        &crate::federation::load().members,
        now,
    );
    let body = serde_json::to_string(&graph).unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string());
    // A local dashboard is a `file://` page (origin `null`), so it can only
    // read this if the reply says so — and only from loopback.
    if from_loopback {
        respond_json_cors(stream, 200, &body);
    } else {
        respond_json(stream, 200, &body);
    }
}
