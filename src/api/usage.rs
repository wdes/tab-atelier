// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `/tabs/usage` read-model: per-tab RSS, token usage, CPU%, connections
//! and egress — an ETag-cached projection parallel to `/tabs`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::{TabSnapshot, respond_with_etag};

pub(super) fn run<W: Write>(
    stream: &mut W,
    state: &Arc<Mutex<TabSnapshot>>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    #[derive(Serialize)]
    struct UsageTab {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resident_memory_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<crate::TokenUsage>,
        cpu_percent: f64,
        connections: usize,
        tx_bytes: u64,
    }
    let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // CPU% is sampled by the `energy` feature (`state.power`, parallel to
    // `state.tabs` by index); 0.0 when the binary was built without it.
    // Isolated to this closure so the usage projection below stays
    // feature-agnostic — the endpoint's shape doesn't depend on `energy`.
    #[cfg(feature = "energy")]
    let cpu_of = |i: usize| state.power.get(i).map_or(0.0, |p| p.cpu_percent);
    #[cfg(not(feature = "energy"))]
    let cpu_of = |_i: usize| 0.0_f64;
    let usage: Vec<UsageTab> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| UsageTab {
            id: t.id.to_string(),
            name: t.name.to_string(),
            resident_memory_bytes: t.resident_memory_bytes,
            tokens: t.tokens,
            cpu_percent: cpu_of(i),
            connections: t.connections,
            tx_bytes: t.tx_bytes,
        })
        .collect();
    let body = serde_json::to_string_pretty(&usage).unwrap_or_default();
    drop(state);
    respond_with_etag(
        stream,
        200,
        "application/json",
        body.as_bytes(),
        accept_gzip,
        if_none_match,
        "",
    );
}
