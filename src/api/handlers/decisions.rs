// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `decisions` route handlers: the KIOSK cross-project decision read-model (PD2) + the
//! Lu/Tranché write path. The panel renders the SERVER read-model VERBATIM (no JS
//! re-gate — the fold in `cli::decision` owns state/verdict/visibility), exactly the
//! catalogue's contract. Archiving the `files[]` is PD3; `tranch` only transits state.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{TabSnapshot, error_json, respond_json};

/// `GET /decisions[?includeArchived]` — the folded cross-project decision read-model
/// (PD1 fold, camelCase). READ-ONLY. `?includeArchived` surfaces archived decisions
/// (`state:archived`); the default hides them. A missing log reads empty. Returns 200
/// `{decisions:[…]}`.
pub(in crate::api) fn list<S: Write>(stream: &mut S, include_archived: bool) {
    let decisions = crate::cli::decision::read_decisions(include_archived);
    let body = serde_json::to_string(&serde_json::json!({ "decisions": decisions })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /decisions/{id}/{read|tranch}` (PD2) — the KIOSK state mutations. Each APPENDS
/// an event (event-sourced; the fold derives the new state), UNDER THE DAEMON LOCK
/// (single-writer — the same lock the catalogue holds, so all cold-source writers
/// serialise) + a read-back gate (append → re-read → confirm our event is the latest
/// for this id → 200). `tranch` requires a non-empty `{verdict}` (a ruling without a
/// verdict is meaningless — mirrors the CLI's `--verdict` requirement). Optional
/// `{by}`. Archiving the `files[]` is PD3.
pub(in crate::api) fn mutate<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    use crate::cli::decision::{
        DecisionEvent, DecisionKind, append_line, archive_decision, decisions_path, outbox_base, parse_decisions,
    };

    #[derive(serde::Deserialize, Default)]
    struct MarkBody {
        verdict: Option<String>,
        by: Option<String>,
    }

    let Some((id_enc, verb)) = p.strip_prefix("/decisions/").and_then(|rest| rest.rsplit_once('/')) else {
        error_json(stream, 404, "bad decisions path");
        return;
    };
    let id = String::from_utf8_lossy(&crate::api_ws::percent_decode(id_enc)).into_owned();
    if id.trim().is_empty() {
        error_json(stream, 400, "empty decision id");
        return;
    }
    let kind = match verb {
        "read" => DecisionKind::Read,
        "tranch" => DecisionKind::Tranched,
        _ => {
            error_json(stream, 404, "unknown decision verb");
            return;
        }
    };

    let body: MarkBody = serde_json::from_slice(body_bytes).unwrap_or_default();
    let verdict = body.verdict.filter(|v| !v.trim().is_empty());
    if kind == DecisionKind::Tranched && verdict.is_none() {
        error_json(stream, 400, "decision tranch: a non-empty verdict is required");
        return;
    }
    let now = crate::unix_millis() / 1000;
    let ev = DecisionEvent { id: id.clone(), kind, at: now, by: body.by, verdict, ..Default::default() };
    let path = decisions_path();

    // Under the daemon lock: append the state event, then (on tranch) ARCHIVE — the
    // ruling triggers filing the bundle under _archive/AAAA-MM/ + appending the `archived`
    // event (PD3), so the decision leaves the active list (reversible via a re-open). The
    // read-back gate confirms the FINAL event landed for this id — `archived` after a
    // tranch, else our own event — a true read-back independent of the folded state.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if append_line(&path, &ev).is_err() {
        drop(guard);
        error_json(stream, 500, "decision: append failed");
        return;
    }
    let expected = if kind == DecisionKind::Tranched {
        // The state transition is recorded even if no file moved; a hard move I/O error
        // 500s (the `tranched` event stands — the ruling isn't lost).
        if let Err(e) = archive_decision(&path, &outbox_base(), &id, now) {
            drop(guard);
            error_json(stream, 500, &format!("decision tranch: archive failed — {e}"));
            return;
        }
        DecisionKind::Archived
    } else {
        kind
    };
    let landed = std::fs::read_to_string(&path)
        .is_ok_and(|body| parse_decisions(&body).iter().rev().find(|e| e.id == id).is_some_and(|e| e.kind == expected));
    drop(guard);
    if landed {
        respond_json(stream, 200, &format!(r#"{{"{verb}":"{id}"}}"#));
    } else {
        error_json(stream, 500, "decision: read-back failed");
    }
}
