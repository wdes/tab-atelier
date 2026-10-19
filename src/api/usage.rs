// SPDX-License-Identifier: MPL-2.0

//! The `/tabs/usage` read-model: per-tab RSS, token usage, CPU%, connections
//! and egress — an ETag-cached projection parallel to `/tabs`.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Conditional, cached};

use super::TabSnapshot;

/// One tab's resource picture.
///
/// The two optional axes are skipped rather than sent as `null` because their
/// absence is meaningful: a tab with no `/proc` sample has no resident memory to
/// report, and one that has never called a model has no token counts. A caller
/// rendering "—" for a missing sample and 0 for a real zero needs the two to be
/// distinguishable on the wire.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UsageTab {
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

/// `GET /tabs/usage`, asked conditionally.
///
/// The body is unchanged from the hand-rolled `ETag` writer this replaces, down to
/// the pretty-printing: a client that already parses this keeps working, and the
/// tag is still the CRC of the *identity* body, so an in-flight revalidation
/// across the deploy answers 304 rather than re-fetching.
pub(super) fn run(state: &Arc<Mutex<TabSnapshot>>) -> Vec<UsageTab> {
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
    drop(state);
    usage
}

/// The conditional reply for this route: the pretty JSON body, tagged.
///
/// Separate from [`run`] because the tag is computed from the *serialized* bytes
/// — deriving it before serialization would mean hashing something a caller
/// never receives, and the shared `cached` helper needs the final body anyway.
pub(super) fn reply(usage: &[UsageTab], conditional: Conditional<'_>) -> tab_atelier_api::Reply {
    let body = serde_json::to_string_pretty(&usage).unwrap_or_default();
    let etag = format!("{:08x}", crate::crc32(body.as_bytes()));
    cached(200, "application/json", bytes::Bytes::from(body), &etag, conditional)
}
