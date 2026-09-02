// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `catalog` route handlers: the retired-agent read-model (RB2) + the LIVE retire
//! write path (RB-wire).

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, parse_tab_key, resolve_tab_idx, respond_json};

/// `GET /catalog/list` — the RETIRED read-model (RB2): every archived card,
/// folded latest-per-slug + usageCount aggregated (RC1). READ-ONLY — a retired
/// card is INERT (only card fields; no lease/status/claimed@peer). A missing
/// catalogue reads as an empty list. Returns 200 `{retired:[…]}`.
pub(in crate::api) fn list<S: Write>(stream: &mut S, include_deleted: bool) {
    let retired = crate::cli::catalog::read_retired();
    // SV3: the v2 SKILL read-model alongside the legacy slug-folded `retired` list —
    // v2 records only (v1 quarantined), folded by skill name with derived metrics.
    // SC1b (#39): `?includeDeleted` ALSO surfaces tombstoned skills (marked
    // `deleted:true`) so the Restore action is reachable; the default still hides them.
    let skills = if include_deleted {
        crate::cli::catalog::read_skill_profiles_all()
    } else {
        crate::cli::catalog::read_skill_profiles()
    };
    let body = serde_json::to_string(&serde_json::json!({ "retired": retired, "skills": skills })).unwrap_or_default();
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
        /// SV2: the éval-à-3 inputs (base prompt + the three votes). When present on a
        /// v2 retire, the daemon runs the eval to DERIVE the outcome + resulting prompt.
        eval: Option<crate::cli::catalog::EvalInput>,
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
        // SV2: run the a-priori éval-à-3 when the votes are supplied. The daemon
        // derives the task LITERALS from the tab's PRECISE context (FN2 ENFORCED, not a
        // trusted declared-clean set) and evaluates the archived bilan → the DERIVED
        // outcome + the resulting (improved / statu-quo) prompt land on the record,
        // overriding any self-report. CF1 then gates the close on a non-empty prompt.
        if let Some(mut eval_in) = req.eval {
            eval_in.task_literals = crate::cli::catalog::task_literals_of(&card);
            eval_in.bilan = card.bilan.clone().unwrap_or_default();
            card = card.with_eval(crate::cli::catalog::evaluate(&eval_in));
        }
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

/// `POST /catalog/{skill}/{edit|delete|restore}` (SC1 #39) — the dashboard catalogue
/// MUTATIONS. Each is an APPEND (event-sourced); the read-model fold derives the new
/// state. The whole read-modify-append runs UNDER THE DAEMON LOCK (single-writer,
/// atomic — borne 3, the same lock retire holds) + a read-back gate (append → re-read →
/// confirm → 200, like `perform_retire`).
///
/// - `edit` — body `{specialty?, prompt?, conventions?, promptVersion?}`. Absent fields
///   carry from the latest content; `promptVersion++`. CF1 (borne 4): a result with an
///   empty skill/prompt → 409. A stale `promptVersion` → 409 (concurrent edit). No such
///   skill → 404. Never touches visibility.
/// - `delete` — append a tombstone (STICKY). `restore` — the EXPLICIT un-tombstone (the
///   only resurrection). A delete/restore of a nonexistent skill is a 200 no-op.
pub(in crate::api) fn mutate<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    use crate::cli::catalog::{
        EditBody, EditError, RecordKind, append_catalog_line, catalog_path, last_visibility_for, latest_content_for,
        plan_edit, read_catalog_cards_at, skill_exists, visibility_record,
    };

    let Some((skill_enc, verb)) = p.strip_prefix("/catalog/").and_then(|rest| rest.rsplit_once('/')) else {
        error_json(stream, 404, "bad catalog path");
        return;
    };
    let skill = String::from_utf8_lossy(&crate::api_ws::percent_decode(skill_enc)).into_owned();
    if skill.trim().is_empty() {
        error_json(stream, 400, "empty skill name");
        return;
    }
    let now = crate::unix_millis();
    let cat = catalog_path();

    // Under the daemon lock: read → modify → append → read-back, ATOMIC + single-writer
    // (retire holds this same lock, so all catalogue writers serialise).
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cards = read_catalog_cards_at(&cat);

    match verb {
        "edit" => {
            let body: EditBody = serde_json::from_slice(body_bytes).unwrap_or_default();
            match plan_edit(latest_content_for(&cards, &skill), &skill, &body, now) {
                Ok(rec) => {
                    let want = rec.prompt_version;
                    if append_catalog_line(&cat, &rec).is_err() {
                        drop(guard);
                        error_json(stream, 500, "catalog edit: append failed");
                        return;
                    }
                    // Read-back gate: the latest content now carries the bumped version.
                    let landed =
                        latest_content_for(&read_catalog_cards_at(&cat), &skill).and_then(|c| c.prompt_version);
                    drop(guard);
                    if landed == want {
                        respond_json(
                            stream,
                            200,
                            &format!(r#"{{"edited":"{skill}","promptVersion":{}}}"#, want.unwrap_or(0)),
                        );
                    } else {
                        error_json(stream, 500, "catalog edit: read-back failed");
                    }
                }
                Err(EditError::NotFound) => {
                    drop(guard);
                    error_json(stream, 404, "catalog edit: no such skill");
                }
                Err(EditError::EmptyProfile) => {
                    drop(guard);
                    error_json(stream, 409, "catalog edit: skill/prompt must stay non-empty (CF1)");
                }
                Err(EditError::Conflict) => {
                    drop(guard);
                    error_json(
                        stream,
                        409,
                        "catalog edit: stale promptVersion — a concurrent edit landed first",
                    );
                }
            }
        }
        verb @ ("delete" | "restore") => {
            let kind = if verb == "delete" {
                RecordKind::Delete
            } else {
                RecordKind::Restore
            };
            if !skill_exists(&cards, &skill) {
                drop(guard);
                respond_json(stream, 200, &format!(r#"{{"{verb}":"{skill}","noop":true}}"#));
                return;
            }
            if append_catalog_line(&cat, &visibility_record(&skill, kind, now)).is_err() {
                drop(guard);
                error_json(stream, 500, "catalog visibility: append failed");
                return;
            }
            let landed = last_visibility_for(&read_catalog_cards_at(&cat), &skill) == Some(kind);
            drop(guard);
            if landed {
                respond_json(stream, 200, &format!(r#"{{"{verb}":"{skill}"}}"#));
            } else {
                error_json(stream, 500, "catalog visibility: read-back failed");
            }
        }
        _ => {
            drop(guard);
            error_json(stream, 404, "unknown catalog verb");
        }
    }
}
