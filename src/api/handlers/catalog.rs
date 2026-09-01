// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `catalog` route handlers: the retired-agent read-model (RB2) + the LIVE retire
//! write path (RB-wire).

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

/// `GET /catalog/list` — the RETIRED read-model (RB2): every archived card,
/// folded latest-per-slug + usageCount aggregated (RC1). READ-ONLY — a retired
/// card is INERT (only card fields; no lease/status/claimed@peer). A missing
/// catalogue reads as an empty list. Returns 200 `{retired:[…]}`.
pub(in crate::api) fn list<S: Write>(stream: &mut S) {
    let retired = crate::cli::catalog::read_retired();
    // SV3: the v2 SKILL read-model alongside the legacy slug-folded `retired` list —
    // v2 records only (v1 quarantined), folded by skill name with derived metrics.
    let skills = crate::cli::catalog::read_skill_profiles();
    let body =
        serde_json::to_string(&serde_json::json!({ "retired": retired, "skills": skills })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{id}/retire` — the LIVE retire WRITE path (RB-wire): the trigger
/// that finally calls `perform_retire` with REAL seams, closing the built≠wired gap.
///
/// Gated fail-closed (RB3): (3a) the tab must be at `rehome_status == safe-to-close`,
/// and (3b) the card is ARCHIVED to catalog.jsonl then RE-READ (read-back) before any
/// close. On a complete archive the tab is de-registered (`deregister_atomic`, a
/// no-op if it wasn't persisted yet) and its close is queued (the owner loop kills
/// the PTY). Otherwise 409 `RETIRE INCOMPLET` and the tab is KEPT. Body: optional
/// `{after_action}` (the RB3 lastMission).
pub(in crate::api) fn retire<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &[u8], path: &str) {
    use crate::cli::catalog::{
        CatalogCard, RetireOutcome, append_catalog_line, catalog_path, deregister_atomic, perform_retire, read_back,
    };

    // Body: the optional RB3 `after_action` + the optional SV3 v2 stamp (the
    // orchestrator's distilled profile + this instance's per-mode telemetry). No
    // `skill` ⇒ a v1 record, byte-identical to before (RB-wire preserved).
    #[derive(serde::Deserialize, Default)]
    struct RetireRequest {
        after_action: Option<String>,
        /// SV1: the structured bilan (retrospective on the prompt). Supersedes
        /// `after_action` as the closing record when present.
        bilan: Option<crate::cli::catalog::Bilan>,
        #[serde(flatten)]
        stamp: crate::cli::catalog::V2Stamp,
    }

    let Some((key_raw, is_uuid)) = parse_tab_key(path, "/retire") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let req: RetireRequest = serde_json::from_slice(p).unwrap_or_default();
    let after_action = req.after_action;
    let now = crate::unix_millis();

    let mut guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&guard, key_raw, is_uuid) else {
        drop(guard);
        error_json(stream, 404, "tab not found");
        return;
    };
    // Snapshot the gate inputs + build the card, then the tab borrow ends.
    let tab = &guard.tabs[idx];
    let ack = tab.rehome_status.as_deref() == Some("safe-to-close");
    let had_session = tab.agent_session_id.is_some();
    let id = tab.id.to_string();
    let mut card = CatalogCard::from_snapshot(tab, after_action, now);
    // SV1: a structured bilan (retrospective on the prompt) supersedes the 1-line
    // after-action as the closing record — captured here at archive time, BEFORE the
    // close and before the éval (SV2). Empty / absent ⇒ the after-action stands.
    if let Some(bilan) = req.bilan {
        card = card.with_bilan(bilan);
    }
    // SV3: an orchestrator that named a `skill` promotes this to a v2 record (profile
    // + per-mode telemetry). Absent ⇒ the card stays v1 (quarantined from the v2
    // read-model). The baseline (session_id/agent_kind) is untouched, A/B-isolated.
    if req.stamp.is_v2() {
        card = card.with_v2(req.stamp);
    }

    let cat = catalog_path();
    let config_base = crate::platform::config_base_dir();
    let outcome = perform_retire(
        &card,
        ack,
        had_session,
        |c| append_catalog_line(&cat, c), // REAL archive to catalog.jsonl
        |rid| read_back(&cat, rid),       // REAL read-back (proof, not "I wrote it")
        || match deregister_atomic(&config_base, &id) {
            // Durable removal from tabs.json; a snapshot-only tab isn't persisted
            // yet → NotFound is a no-op (nothing to de-register), not a failure.
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
        || Ok(()), // the irreversible PTY close is queued below (owner loop)
    );

    match outcome {
        RetireOutcome::Retired => {
            guard.pending_closes.push(idx); // the owner loop kills the PTY + persists
            drop(guard);
            respond_json(stream, 200, &format!(r#"{{"retired":"{id}"}}"#));
        }
        RetireOutcome::Incomplete(flag) => {
            drop(guard);
            error_json(stream, 409, flag); // RETIRE INCOMPLET — tab kept
        }
        RetireOutcome::CloseFailed => {
            drop(guard);
            error_json(stream, 500, "retire: de-register failed — tab kept");
        }
    }
}
