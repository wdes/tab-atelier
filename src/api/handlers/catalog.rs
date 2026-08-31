// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `catalog` route handler (RB2): the retired-agent read-model.

use std::io::Write;

use super::super::respond_json;

/// `GET /catalog/list` — the RETIRED read-model (RB2): every archived card,
/// folded latest-per-slug + usageCount aggregated (RC1). READ-ONLY — a retired
/// card is INERT (only card fields; no lease/status/claimed@peer). A missing
/// catalogue reads as an empty list. Returns 200 `{retired:[…]}`.
pub(in crate::api) fn list<S: Write>(stream: &mut S) {
    let retired = crate::cli::catalog::read_retired();
    let body = serde_json::to_string(&serde_json::json!({ "retired": retired })).unwrap_or_default();
    respond_json(stream, 200, &body);
}
