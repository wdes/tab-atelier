// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `decisions` route handlers: the KIOSK cross-project decision read-model (PD2) + the
//! Lu/Tranché write path. The panel renders the SERVER read-model VERBATIM (no JS
//! re-gate — the fold in `cli::decision` owns state/verdict/visibility). Archiving the
//! `files[]` is PD3; `tranch` only transits state.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_bytes, respond_json};

/// `GET /decisions/file?path=<abs>` — serve a decision bundle's CONTENT, SANDBOXED to
/// `<outbox>` and its `_archive/` subtree (#kiosk).
///
/// The KIOSK panel links point here: a raw outbox path 401s at the daemon, so this route
/// lets the PO READ a bundle we SHOW them. Every path is `~`-expanded then CANONICALIZED
/// (collapsing `..` and symlinks) and must live under the canonicalized outbox — anything
/// outside (the source tree, `~/.ssh`, `/etc/…`) is refused 403. Served as text/plain.
/// READ-ONLY.
pub(super) fn file<S: Write>(stream: &mut S, path_q: Option<&str>) {
    let Some(raw) = path_q.filter(|s| !s.trim().is_empty()) else {
        error_json(stream, 400, "decisions file: ?path= is required");
        return;
    };
    let requested = raw.strip_prefix("~/").map_or_else(
        || std::path::PathBuf::from(raw),
        |rest| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
    );
    // Canonicalize both sides so the sandbox check can't be walked out of (`..`, symlink).
    // A non-existent file / unreadable outbox → 404 (never leak whether a path exists
    // outside the sandbox — the confinement check runs on the canonical form first).
    let (Ok(canon), Ok(base)) = (
        std::fs::canonicalize(&requested),
        std::fs::canonicalize(crate::cli::decision::outbox_base()),
    ) else {
        error_json(stream, 404, "decisions file: not found");
        return;
    };
    if !canon.starts_with(&base) {
        error_json(stream, 403, "decisions file: outside the outbox sandbox");
        return;
    }
    if !canon.is_file() {
        error_json(stream, 404, "decisions file: not a file");
        return;
    }
    match std::fs::read(&canon) {
        Ok(bytes) => respond_bytes(stream, 200, "text/plain; charset=utf-8", &bytes),
        Err(_) => error_json(stream, 404, "decisions file: unreadable"),
    }
}

/// `GET /decisions[?includeArchived]` — the folded cross-project decision read-model
/// (PD1 fold, camelCase). READ-ONLY. `?includeArchived` surfaces archived decisions
/// (`state:archived`); the default hides them. A missing log reads empty. Returns 200
/// `{decisions:[…]}`.
pub(super) fn list<S: Write>(stream: &mut S, include_archived: bool) {
    let decisions = crate::cli::decision::read_decisions(include_archived);
    let body = serde_json::to_string(&serde_json::json!({ "decisions": decisions })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /decisions/{id}/{read|tranch}` (PD2) — the KIOSK state mutations. Each APPENDS
/// an event (event-sourced; the fold derives the new state), UNDER THE DAEMON LOCK
/// (single-writer — all cold-source writers serialise) + a read-back gate (append →
/// re-read → confirm our event is the latest for this id → 200). `tranch` requires a
/// non-empty `{verdict}` (a ruling without a verdict is meaningless — mirrors the CLI's
/// `--verdict` requirement). Optional `{by}`. Archiving the `files[]` is PD3.
pub(super) fn mutate<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
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
    let ev = DecisionEvent {
        id: id.clone(),
        kind,
        at: now,
        by: body.by,
        verdict,
        ..Default::default()
    };
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
    let landed = std::fs::read_to_string(&path).is_ok_and(|body| {
        parse_decisions(&body)
            .iter()
            .rev()
            .find(|e| e.id == id)
            .is_some_and(|e| e.kind == expected)
    });
    drop(guard);
    if landed {
        respond_json(stream, 200, &format!(r#"{{"{verb}":"{id}"}}"#));
    } else {
        error_json(stream, 500, "decision: read-back failed");
    }
}
