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

use std::io::Write;

use super::{error_json, respond_json};

/// Cap on a single merge batch. Gossip is anti-entropy, not bulk transfer —
/// an oversized push is either a bug or an attempt to fill the disk.
pub const MAX_MERGE: usize = 5_000;

/// `GET /blackboard?since=N` — entries from position `N` on.
///
/// `since` is a position in *this host's* file, which is all a puller needs to
/// resume; it is deliberately not a logical clock, because the merge is a set
/// union and does not need one.
pub(super) fn list<W: Write>(stream: &mut W, since: Option<usize>) {
    let notes = crate::cli::team::read_blackboard();
    let from = since.unwrap_or(0).min(notes.len());
    let body = serde_json::json!({
        "entries": &notes[from..],
        "next": notes.len(),
        "origin": crate::cli::team::origin_id(),
    })
    .to_string();
    respond_json(stream, 200, &body);
}

/// `POST /blackboard` — merge a peer's entries, returning how many were new.
///
/// Idempotent by construction: entries we already hold are dropped by id, so a
/// peer that re-sends its whole log (after a restart, say) costs one scan and
/// changes nothing.
pub(super) fn merge<W: Write>(stream: &mut W, body_bytes: &[u8]) {
    let parsed: serde_json::Value = serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
    let Some(entries) = parsed.get("entries").and_then(|v| v.as_array()) else {
        error_json(stream, 400, "expected {\"entries\":[…]}");
        return;
    };
    if entries.len() > MAX_MERGE {
        error_json(stream, 413, &format!("batch larger than {MAX_MERGE} entries"));
        return;
    }
    let incoming: Vec<crate::cli::team::Note> = entries
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    match crate::cli::team::merge_into_blackboard(&incoming) {
        Ok(added) => {
            let body = serde_json::json!({ "merged": added, "received": incoming.len() }).to_string();
            respond_json(stream, 200, &body);
        }
        Err(e) => error_json(stream, 500, &e),
    }
}
