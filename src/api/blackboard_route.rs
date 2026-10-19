// SPDX-License-Identifier: MPL-2.0

//! The blackboard resource — read entries, and accept a peer's.
//!
//! This is the whole cross-host story. The log is a grow-only set keyed by
//! entry id, so merging is a union: idempotent, commutative, associative. Two
//! daemons that exchange batches in any order, any number of times, converge
//! on the same board (Shapiro et al., CRDTs, 2011).
//!
//! Nothing here needs a quorum, an election or a clock that agrees with
//! anyone else's, which is precisely why the fleet can span hosts without a
//! consensus protocol.

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

/// Cap on a single merge batch. Gossip is anti-entropy, not bulk transfer —
/// an oversized push is either a bug or an attempt to fill the disk.
pub const MAX_MERGE: usize = 5_000;

/// A slice of the board, with where the next pull should resume.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Slice {
    entries: Vec<crate::cli::team::Note>,
    next: usize,
    origin: String,
}

/// What a merge changed: how much arrived, and how much of it was new.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Merged {
    merged: usize,
    received: usize,
}

/// `GET /blackboard?since=N` — entries from position `N` on.
///
/// `since` is a position in *this host's* file, which is all a puller needs to
/// resume; it is deliberately not a logical clock, because the merge is a set
/// union and does not need one.
pub(super) fn list(request: &Request<'_>) -> Doc<Slice> {
    let since: Option<usize> = request.query_value("since").and_then(|v| v.trim().parse().ok());
    let notes = crate::cli::team::read_blackboard();
    let from = since.unwrap_or(0).min(notes.len());
    Doc::ok(Slice {
        entries: notes[from..].to_vec(),
        next: notes.len(),
        origin: crate::cli::team::origin_id(),
    })
}

/// `POST /blackboard` — merge a peer's entries, returning how many were new.
///
/// Idempotent by construction: entries we already hold are dropped by id, so a
/// peer that re-sends its whole log (after a restart, say) costs one scan and
/// changes nothing.
pub(super) fn merge(request: &Request<'_>) -> Result<Doc<Merged>, Failure> {
    let parsed: serde_json::Value = serde_json::from_slice(request.body).unwrap_or(serde_json::Value::Null);
    let Some(entries) = parsed.get("entries").and_then(|v| v.as_array()) else {
        return Err(Failure::problem(400, "expected {\"entries\":[…]}"));
    };
    if entries.len() > MAX_MERGE {
        return Err(Failure::problem(413, format!("batch larger than {MAX_MERGE} entries")));
    }
    let incoming: Vec<crate::cli::team::Note> = entries
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    let added = crate::cli::team::merge_into_blackboard(&incoming).map_err(|e| Failure::problem(500, e))?;
    Ok(Doc::ok(Merged {
        merged: added,
        received: incoming.len(),
    }))
}
