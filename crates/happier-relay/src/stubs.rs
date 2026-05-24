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
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use base64::Engine;
use serde::Deserialize;
use sqlx::Row;

use crate::auth::UserId;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // limit / after_seq / scope reserved for paging
pub struct TabMessagesQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub after_seq: Option<i64>,
    #[serde(default)]
    pub before_seq: Option<i64>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub roles: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

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

/// Fetch an artifact row by id, scoped to `user_id`, and return its
/// decoded header + raw body when the artifact represents a tab
/// (header.kind == "tab-atelier:tab"). Returns `Ok(None)` for any
/// non-tab artifact so the caller can fall through to the regular
/// sessions handler. Sql / decode errors propagate as `Err`.
async fn load_tab_artifact(
    state: &AppState,
    user_id: &str,
    id: &str,
) -> Result<Option<(serde_json::Value, Vec<u8>, i64, i64)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT header, body, created_at, updated_at
         FROM artifacts WHERE id = ?1 AND account_id = ?2",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let header_b64: Vec<u8> = row.get("header");
    let body: Vec<u8> = row.get("body");
    let created_at: i64 = row.get("created_at");
    let updated_at: i64 = row.get("updated_at");
    let b64 = base64::engine::general_purpose::STANDARD;
    let header_json: serde_json::Value = b64
        .decode(&header_b64)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null);
    if header_json.get("kind").and_then(serde_json::Value::as_str) != Some("tab-atelier:tab") {
        return Ok(None);
    }
    Ok(Some((header_json, body, created_at, updated_at)))
}

/// `GET /v1/sessions/{id}/messages` — tab variant.
///
/// Returns a single synthetic `ApiMessage` whose `content.v` carries
/// the tab's scrollback wrapped in the `RawRecord` shape the mobile
/// UI's `normalizeRawMessage` accepts (an agent→assistant text
/// block). Prompt-history calls (`role=user`) get an empty page so
/// the UI back-pagination terminates without offering "previous
/// prompts" that don't exist.
///
/// Returns `Err(404, {error:"not-a-tab-artifact"})` for ids that
/// aren't tab artifacts; the dispatcher in main.rs reads that
/// sentinel to chain to the existing `sessions::list_messages`.
pub async fn list_tab_messages_v1(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
    Query(q): Query<TabMessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some((header, body, created_at, updated_at)) =
        load_tab_artifact(&state, &user.0, &session_id).await.map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
        })?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not-a-tab-artifact" })),
        ));
    };

    let wants_user_only = q.role.as_deref() == Some("user")
        || q.roles
            .as_deref()
            .is_some_and(|s| s.split(',').all(|r| r.trim() == "user"));
    if wants_user_only || q.before_seq.is_some_and(|b| b <= 1) {
        return Ok(Json(serde_json::json!({
            "messages": [],
            "hasMore": false,
            "nextBeforeSeq": null,
            "nextAfterSeq": null,
        })));
    }

    // The on-disk body keeps full ANSI so the relay's own web UI can
    // colour it. The mobile UI renders message text plain — without
    // stripping, scrollback shows literal `^[[31m` everywhere.
    let raw = String::from_utf8_lossy(&body);
    let text = strip_ansi(&raw);
    let name = header
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tab")
        .to_string();

    // RawRecord agent-output envelope — `normalizeRawMessage` collapses
    // this into an `agent-text` Message that renders as a monospace
    // assistant bubble.
    let content_v = serde_json::json!({
        "role": "agent",
        "content": {
            "type": "output",
            "data": {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }],
                },
            },
        },
        "meta": { "displayName": name },
    });

    let msg = serde_json::json!({
        "id": format!("{session_id}:scrollback"),
        "seq": 1,
        "localId": null,
        "sidechainId": null,
        "messageRole": "agent",
        "content": { "t": "plain", "v": content_v },
        "createdAt": created_at,
        "updatedAt": updated_at,
    });

    Ok(Json(serde_json::json!({
        "messages": [msg],
        "hasMore": false,
        "nextBeforeSeq": null,
        "nextAfterSeq": null,
    })))
}

/// `GET /v2/sessions/{id}` — tab variant. Returns the full session
/// detail object the mobile fetches when opening a chat view. Matches
/// the `V2SessionByIdResponse` shape (`encryptionMode: "plain"`, no DEK,
/// no share). Falls through to `sessions::get_one` (via the
/// dispatcher in main.rs) for non-tab ids.
pub async fn get_tab_session_v2(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some((header, _body, created_at, updated_at)) =
        load_tab_artifact(&state, &user.0, &session_id).await.map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
        })?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not-a-tab-artifact" })),
        ));
    };

    let name = header
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("(unnamed tab)")
        .to_string();
    let metadata = serde_json::json!({
        "name": &name,
        "summary": { "text": &name, "updatedAt": updated_at },
        "path": &name,
        "host": "tab-atelier",
    })
    .to_string();

    Ok(Json(serde_json::json!({
        "session": {
            "id": session_id,
            "seq": 1,
            "encryptionMode": "plain",
            "metadata": metadata,
            "metadataVersion": 1,
            "agentState": null,
            "agentStateVersion": 0,
            "dataEncryptionKey": null,
            "lastViewedSessionSeq": 1,
            "pendingPermissionRequestCount": 0,
            "pendingUserActionRequestCount": 0,
            "share": null,
            "archivedAt": null,
            "active": true,
            "activeAt": updated_at,
            "createdAt": created_at,
            "updatedAt": updated_at,
            "pendingCount": 0,
            "pendingVersion": 0,
            "lastMessage": null,
        }
    })))
}

/// `GET /v2/sessions/{id}/pending` — always empty for tabs.
pub async fn tab_session_pending(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "pending": [] }))
}

/// `GET /v2/session-folder-assignments?sessionIds=...` — empty.
pub async fn session_folder_assignments() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "assignments": [] }))
}

/// Strip ANSI escape sequences from a terminal scrollback string so
/// the mobile renders plain text. Handles:
/// - CSI: `ESC [ params final` (colors, cursor moves, erase, etc.)
/// - OSC: `ESC ] params ST|BEL` (window titles, OSC-8 hyperlinks)
/// - Single-char ESC sequences: `ESC <char>` (e.g. `ESC =`, `ESC >`)
///
/// Mirrors the semantics of `tab_atelier::strip_ansi` (which the
/// desktop uses for the clipboard-as-plaintext path) plus OSC
/// handling, since terminal apps (Claude Code, ripgrep) often emit
/// OSC-8 links the mobile shouldn't show literally.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some(&'[') => {
                // CSI — consume params/intermediates until a final
                // byte in `0x40..=0x7e`.
                chars.next();
                for nc in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&nc) {
                        break;
                    }
                }
            }
            Some(&']') => {
                // OSC — terminated by ST (`ESC \`) or BEL (`\x07`).
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '\x07' {
                        break;
                    }
                    if nc == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                // Two-char escape sequence (`ESC =`, `ESC >`, …);
                // drop the next char and move on.
                chars.next();
            }
            None => {
                // Lone trailing ESC. Drop it.
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_sgr_colors() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m world"), "hello world");
    }

    #[test]
    fn strips_cursor_moves_and_erase() {
        assert_eq!(strip_ansi("\x1b[2Jabc\x1b[1;1H"), "abc");
    }

    #[test]
    fn strips_osc_hyperlink() {
        // OSC 8 hyperlink — both BEL- and ST-terminated forms.
        assert_eq!(strip_ansi("\x1b]8;;https://x\x07link\x1b]8;;\x07"), "link");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\rest"), "rest");
    }

    #[test]
    fn passes_plain_text_through() {
        let s = "no escapes here\nsecond line\ttabbed";
        assert_eq!(strip_ansi(s), s);
    }

    #[test]
    fn drops_lone_escape() {
        assert_eq!(strip_ansi("a\x1bb"), "a"); // ESC b = two-char escape, both consumed
        assert_eq!(strip_ansi("trailing\x1b"), "trailing");
    }
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
