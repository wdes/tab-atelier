// SPDX-License-Identifier: MPL-2.0

//! `catalog` route handler: the PROFIL (v2) read-model.
//!
//! Ported from MX `d08b0f9e` (`src/api/catalog.rs`), narrowed to the half B needs.
//! The MX handler also served the retired-agent (v1/organisation) read-model and the
//! live retire/mutate write paths — none of which are part of this port.

use std::io::Write;

use super::respond_json;

/// `GET /catalog/list[?includeDeleted]` — the v2 SKILL read-model: every profile,
/// folded by skill name, with per-mode metrics + the derived fresh-vs-resume verdict.
///
/// READ-ONLY: a profile is INERT (no lease/status/claimed@peer). A missing catalogue
/// reads as an empty list. `?includeDeleted` ALSO surfaces tombstoned skills (marked
/// `deleted:true`) so the Restore action stays reachable; the default hides them.
///
/// `retired` is kept, always empty (see the module docs on the port's scope): it is the
/// v1/organisation list, retired by the PO. Dropping the key would change the response
/// SHAPE for a reader that still reads it; an empty list is the honest "no v1 data
/// here" and keeps `GetCatalogListResponse` forward-compatible.
///
/// ponytail: if both consumers JSON.parse, the key can go the day the v1 readers do.
pub(in crate::api) fn list<S: Write>(stream: &mut S, include_deleted: bool) {
    let skills = if include_deleted {
        crate::cli::catalog::read_skill_profiles_all()
    } else {
        crate::cli::catalog::read_skill_profiles()
    };
    let body = serde_json::to_string(&serde_json::json!({
        "retired": Vec::<serde_json::Value>::new(),
        "skills": skills,
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}
