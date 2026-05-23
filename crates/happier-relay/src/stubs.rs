// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Minimal-shape stubs for happier mobile UI endpoints that the
//! app polls during startup and tab navigation. None of these
//! return *real* data yet — they return the smallest payload that
//! parses cleanly, so the UI proceeds instead of showing "offline"
//! / "relay not supported" / spinner-forever errors.
//!
//! Each route's shape was extracted from the call sites in
//! `apps/ui/sources/sync/**` of `happier-dev/happier`. When you
//! want a feature to actually work (e.g. machine list, profile),
//! upgrade the stub here to read from real state.

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use base64::Engine;
use sqlx::Row;

use crate::auth::UserId;
use crate::state::AppState;

/// `GET /v1/machines` — laptop appears as a single "active" machine.
///
/// The mobile UI's `getMachineDisplayName` reads `displayName` →
/// `host` → falls back to `machine.id`. Properly populating
/// `displayName` / `host` requires shipping an encrypted (Curve25519
/// sealed-box + AES-GCM) blob in `metadata`, which can't be done
/// without sharing key material with the device. Until we wire
/// that, leave `metadata` empty (the client skips `decryptMetadata`
/// when metadata is falsy) and put a human-readable hostname into
/// `id` so the fallback path renders something meaningful instead
/// of a literal "tab-atelier-host".
pub async fn list_machines(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now = now_ms();
    Json(serde_json::json!([
        {
            "id": state.machine_id.as_str(),
            "metadata": "",
            "metadataVersion": 1,
            "daemonState": null,
            "daemonStateVersion": 0,
            "dataEncryptionKey": null,
            "seq": 1,
            "active": true,
            "activeAt": now,
            "revokedAt": null,
            "createdAt": now,
            "updatedAt": now,
        }
    ]))
}

/// `GET /v1/account/profile` — empty defaults. The UI's
/// `profileParse` falls back to its built-in `profileDefaults` if
/// the response doesn't validate, but an explicitly-empty payload
/// keeps the network tab cleaner.
pub async fn account_profile() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "id": "shared",
        "timestamp": now_ms(),
        "firstName": null,
        "lastName": null,
        "username": null,
        "avatar": null,
        "linkedProviders": [],
        "connectedServices": [],
        "connectedServicesV2": [],
    }))
}

/// `GET /v1/account/encryption` — we don't run e2ee, so report
/// `plain`. The UI handles both modes; in plain mode it skips
/// encryption work entirely.
pub async fn account_encryption() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "mode": "plain", "updatedAt": 0 }))
}

/// `GET /v2/changes` — incremental sync cursor pagination. Empty
/// payload tells the UI "no changes since your cursor".
pub async fn v2_changes() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "changes": [], "cursor": 0 }))
}

/// `GET /v2/cursor` — the UI reads this to learn its starting
/// cursor before polling `/v2/changes`. Returning 0 means "from
/// the beginning"; we have no real change log anyway.
pub async fn v2_cursor() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "cursor": 0 }))
}

/// `GET /v1/push-tokens` — list registered push tokens. We don't
/// integrate with FCM/APNS, so always empty.
pub async fn list_push_tokens() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "tokens": [] }))
}

/// `POST /v1/push-tokens` — registering is a no-op; the UI just
/// expects 200.
pub async fn register_push_token() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// `DELETE /v1/push-tokens/{token}` — also a no-op.
pub async fn delete_push_token() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// `GET /v1/feed` — social feed page. Always empty (we have no
/// social graph).
pub async fn feed() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "items": [], "cursor": null }))
}

/// `GET /v1/friends` — social friends list. Always empty.
pub async fn friends() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "friends": [] }))
}

/// `GET /v1/account/activity/badge-snapshot` — activity badge state.
/// Empty / no-badge so the UI clears it.
pub async fn activity_badge_snapshot() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "snapshot": null }))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| {
            i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `GET /v2/sessions` — list happier sessions. We don't run real
/// Claude Code sessions, but the mobile UI uses this screen as its
/// home view. Synthesise one session per tab-atelier tab so the
/// user sees their tabs there. Each tab's bridge-published artifact
/// supplies the id, header (with `name`), and timestamps; we wrap
/// that into the `V2SessionRecord` shape from
/// `packages/protocol/src/sessionControl/contract.ts`:
/// `id, seq, createdAt, updatedAt, active, activeAt, metadata,
/// metadataVersion, agentState, agentStateVersion, dataEncryptionKey`.
///
/// `metadata` is plain-text JSON (matches happier's "plain"
/// encryption mode — `parsePlainSessionMetadata` JSON.parses it
/// directly). `dataEncryptionKey: null` tells the client there's
/// no per-session key to fetch.
pub async fn list_sessions_v2(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let rows = sqlx::query(
        "SELECT id, header, seq, created_at, updated_at
         FROM artifacts WHERE account_id = ?1 ORDER BY updated_at DESC",
    )
    .bind(&user.0)
    .fetch_all(&state.db)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal error" })),
        )
    })?;

    let b64 = base64::engine::general_purpose::STANDARD;
    let mut sessions = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let header_b64: Vec<u8> = row.get("header");
        let seq: i64 = row.get("seq");
        let created_at: i64 = row.get("created_at");
        let updated_at: i64 = row.get("updated_at");

        // Tab name lives in the bridge-written header JSON. Pull it
        // out so the mobile UI shows the tab's name rather than the
        // raw UUID.
        let header_json: Option<serde_json::Value> = b64
            .decode(&header_b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let name = header_json
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed tab)")
            .to_string();
        let kind = header_json
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("tab-atelier:tab")
            .to_string();
        // Skip artifacts that aren't tabs (defensive — today the
        // bridge writes only `tab-atelier:tab` artifacts).
        if kind != "tab-atelier:tab" {
            continue;
        }

        // Plain-mode session metadata is raw JSON; the client's
        // `parsePlainSessionMetadata` JSON.parses it directly. The
        // mobile session list renderer reads `name`, `summary.text`,
        // and `path` in that order — without `name` every tab shows
        // as "unknown". Setting all three keeps the list useful.
        let metadata = serde_json::json!({
            "name": name.clone(),
            "summary": {
                "text": name.clone(),
                "updatedAt": updated_at,
            },
            "path": name.clone(),
            "host": "tab-atelier",
        })
        .to_string();

        sessions.push(serde_json::json!({
            "id": id,
            "seq": seq,
            "createdAt": created_at,
            "updatedAt": updated_at,
            "active": true,
            "activeAt": updated_at,
            // Without `encryptionMode: "plain"` the client defaults
            // to "e2ee" and tries to decrypt our plain-JSON metadata
            // — failing silently and rendering the session as
            // "unknown". This selects the documented plain-text
            // branch in `parsePlainSessionPayload.ts`.
            "encryptionMode": "plain",
            "metadata": metadata,
            "metadataVersion": 1,
            "agentState": null,
            "agentStateVersion": 0,
            "dataEncryptionKey": null,
        }));
    }

    Ok(Json(serde_json::json!({
        "sessions": sessions,
        "nextCursor": null,
        "hasNext": false,
    })))
}
