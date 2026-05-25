// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session CRUD + messages endpoints. Matches happier's wire shape
//! closely enough for the mobile client to round-trip — single-tenant
//! simplifications are flagged inline.

use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::UserId;
use crate::state::AppState;

// --- wire types -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionReq {
    pub tag: String,
    pub metadata: String,
    #[serde(default)]
    pub agent_state: Option<String>,
    #[serde(default)]
    pub data_encryption_key: Option<String>,
    #[serde(default)]
    pub encryption_mode: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub seq: i64,
    pub encryption_mode: String,
    pub metadata: Option<String>,
    pub metadata_version: i64,
    pub agent_state: Option<String>,
    pub agent_state_version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_encryption_key: Option<String>,
    pub pending_count: i64,
    pub pending_version: i64,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Always `null` in the spike — happier returns the latest message
    /// envelope summary here but we don't compute it (no UI needs it
    /// until we add the sessions-list rendering tests).
    pub last_message: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchSessionReq {
    #[serde(default)]
    pub metadata: Option<CipherWithVersion>,
    #[serde(default)]
    pub agent_state: Option<CipherWithVersion>,
}

#[derive(Debug, Deserialize)]
pub struct CipherWithVersion {
    pub ciphertext: String,
    #[serde(rename = "expectedVersion")]
    pub expected_version: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    pub id: String,
    pub seq: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidechain_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_role: Option<String>,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostMessageReq {
    #[serde(default)]
    pub ciphertext: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub local_id: Option<String>,
    #[serde(default)]
    pub sidechain_id: Option<String>,
    #[serde(default)]
    pub message_role: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default, rename = "afterSeq")]
    pub after_seq: Option<i64>,
    #[serde(default, rename = "beforeSeq")]
    pub before_seq: Option<i64>,
}

// --- helpers -----------------------------------------------------------------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed())
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    seq: i64,
    encryption_mode: String,
    metadata: Option<String>,
    metadata_version: i64,
    agent_state: Option<String>,
    agent_state_version: i64,
    data_encryption_key: Option<Vec<u8>>,
    active: i64,
    active_at: Option<i64>,
    archived_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl SessionRow {
    fn into_view(self) -> SessionView {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        SessionView {
            id: self.id,
            seq: self.seq,
            encryption_mode: self.encryption_mode,
            metadata: self.metadata,
            metadata_version: self.metadata_version,
            agent_state: self.agent_state,
            agent_state_version: self.agent_state_version,
            data_encryption_key: self.data_encryption_key.as_deref().map(|b| b64.encode(b)),
            pending_count: 0,
            pending_version: 0,
            active: self.active != 0,
            active_at: self.active_at,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_message: None,
        }
    }
}

async fn fetch_session(pool: &SqlitePool, account_id: &str, session_id: &str) -> sqlx::Result<Option<SessionRow>> {
    sqlx::query_as::<_, SessionRow>(
        "SELECT id, seq, encryption_mode, metadata, metadata_version, agent_state, agent_state_version,
                data_encryption_key, active, active_at, archived_at, created_at, updated_at
         FROM sessions WHERE id = ?1 AND account_id = ?2",
    )
    .bind(session_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await
}

// --- handlers ----------------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(req): Json<CreateSessionReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mode = req.encryption_mode.unwrap_or_else(|| "e2ee".to_string());
    if !matches!(mode.as_str(), "e2ee" | "plain") {
        return Err(err(StatusCode::BAD_REQUEST, "invalid encryption mode"));
    }
    let data_key_bytes = match req.data_encryption_key.as_deref() {
        Some(s) => Some(
            b64.decode(s)
                .map_err(|_| err(StatusCode::BAD_REQUEST, "invalid dataEncryptionKey base64"))?,
        ),
        None => None,
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_secs();

    sqlx::query(
        "INSERT INTO sessions (id, account_id, tag, seq, encryption_mode, metadata, metadata_version,
                agent_state, agent_state_version, data_encryption_key, active, active_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, 1, ?6, ?7, ?8, 1, ?9, ?10, ?11)",
    )
    .bind(&id)
    .bind(&user.0)
    .bind(&req.tag)
    .bind(&mode)
    .bind(&req.metadata)
    .bind(req.agent_state.as_deref())
    .bind(i64::from(req.agent_state.is_some()))
    .bind(data_key_bytes.as_deref())
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::warn!(error = ?e, "session insert failed");
        // Unique violation on (account_id, tag) → 409.
        if matches!(&e, sqlx::Error::Database(db) if db.is_unique_violation()) {
            err(StatusCode::CONFLICT, "session with this tag already exists")
        } else {
            err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    })?;

    let row = fetch_session(&state.db, &user.0, &id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "session vanished after insert"))?;
    Ok(Json(serde_json::json!({ "session": row.into_view() })))
}

pub async fn list_all(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let rows: Vec<SessionRow> = sqlx::query_as(
        "SELECT id, seq, encryption_mode, metadata, metadata_version, agent_state, agent_state_version,
                data_encryption_key, active, active_at, archived_at, created_at, updated_at
         FROM sessions WHERE account_id = ?1 AND archived_at IS NULL
         ORDER BY updated_at DESC",
    )
    .bind(&user.0)
    .fetch_all(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let sessions: Vec<SessionView> = rows.into_iter().map(SessionRow::into_view).collect();
    Ok(Json(serde_json::json!({ "sessions": sessions })))
}

pub async fn get_one(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let row = fetch_session(&state.db, &user.0, &session_id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session not found"))?;
    Ok(Json(serde_json::json!({ "session": row.into_view() })))
}

pub async fn patch(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
    Json(req): Json<PatchSessionReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut row = fetch_session(&state.db, &user.0, &session_id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session not found"))?;

    // Optimistic CAS: each field has its own version. On mismatch we
    // surface BOTH the current version and the current ciphertext so
    // the client can rebase + retry. This matches happier's response.
    let mut mismatch: Option<serde_json::Value> = None;
    let now = now_secs();

    if let Some(ref m) = req.metadata {
        if m.expected_version == row.metadata_version {
            row.metadata = Some(m.ciphertext.clone());
            row.metadata_version += 1;
        } else {
            let mut details = serde_json::json!({
                "metadata": {
                    "version": row.metadata_version,
                    "value": row.metadata.clone(),
                }
            });
            if let Some(ref a) = req.agent_state
                && a.expected_version != row.agent_state_version
            {
                details["agentState"] = serde_json::json!({
                    "version": row.agent_state_version,
                    "value": row.agent_state.clone(),
                });
            }
            mismatch = Some(details);
        }
    }
    if mismatch.is_none()
        && let Some(ref a) = req.agent_state
    {
        if a.expected_version == row.agent_state_version {
            row.agent_state = Some(a.ciphertext.clone());
            row.agent_state_version += 1;
        } else {
            mismatch = Some(serde_json::json!({
                "agentState": {
                    "version": row.agent_state_version,
                    "value": row.agent_state.clone(),
                }
            }));
        }
    }

    if let Some(details) = mismatch {
        let mut body = serde_json::json!({ "success": false, "error": "version-mismatch" });
        if let Some(obj) = body.as_object_mut()
            && let serde_json::Value::Object(extra) = details
        {
            obj.extend(extra);
        }
        return Ok(Json(body));
    }

    sqlx::query(
        "UPDATE sessions SET metadata = ?1, metadata_version = ?2, agent_state = ?3, agent_state_version = ?4, updated_at = ?5
         WHERE id = ?6",
    )
    .bind(&row.metadata)
    .bind(row.metadata_version)
    .bind(&row.agent_state)
    .bind(row.agent_state_version)
    .bind(now)
    .bind(&row.id)
    .execute(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    Ok(Json(serde_json::json!({
        "success": true,
        "metadata": req.metadata.as_ref().map(|_| serde_json::json!({ "version": row.metadata_version })),
        "agentState": req.agent_state.as_ref().map(|_| serde_json::json!({ "version": row.agent_state_version })),
    })))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let now = now_secs();
    let result = sqlx::query("UPDATE sessions SET archived_at = ?1, updated_at = ?1 WHERE id = ?2 AND account_id = ?3 AND archived_at IS NULL")
        .bind(now)
        .bind(&session_id)
        .bind(&user.0)
        .execute(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    if result.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "session not found"));
    }
    Ok(Json(serde_json::json!({ "success": true })))
}

// --- messages ---------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: String,
    seq: i64,
    local_id: Option<String>,
    sidechain_id: Option<String>,
    message_role: Option<String>,
    content: String,
    created_at: i64,
    updated_at: i64,
}

impl MessageRow {
    fn into_view(self) -> MessageView {
        MessageView {
            id: self.id,
            seq: self.seq,
            local_id: self.local_id,
            sidechain_id: self.sidechain_id,
            message_role: self.message_role,
            content: self.content,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

pub async fn list_messages(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(session_id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if fetch_session(&state.db, &user.0, &session_id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .is_none()
    {
        return Err(err(StatusCode::NOT_FOUND, "session not found"));
    }
    let limit = q.limit.unwrap_or(150).clamp(1, 500);

    // afterSeq + beforeSeq are mutually exclusive in happier; we honour
    // that here. afterSeq returns rows with seq > after_seq ascending;
    // beforeSeq returns rows with seq < before_seq descending. The
    // client uses the latter for back-pagination.
    let rows: Vec<MessageRow> = match (q.after_seq, q.before_seq) {
        (Some(_), Some(_)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "afterSeq and beforeSeq are mutually exclusive",
            ));
        }
        (Some(after), None) => {
            sqlx::query_as(
                "SELECT id, seq, local_id, sidechain_id, message_role, content, created_at, updated_at
                 FROM session_messages WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )
            .bind(&session_id)
            .bind(after)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        }
        (None, Some(before)) => {
            sqlx::query_as(
                "SELECT id, seq, local_id, sidechain_id, message_role, content, created_at, updated_at
                 FROM session_messages WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
            )
            .bind(&session_id)
            .bind(before)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        }
        (None, None) => {
            sqlx::query_as(
                "SELECT id, seq, local_id, sidechain_id, message_role, content, created_at, updated_at
                 FROM session_messages WHERE session_id = ?1 ORDER BY seq ASC LIMIT ?2",
            )
            .bind(&session_id)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        }
    }
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    let has_more = i64::try_from(rows.len()).is_ok_and(|n| n >= limit);
    let next_after = rows.last().map(|r| r.seq);
    let messages: Vec<MessageView> = rows.into_iter().map(MessageRow::into_view).collect();
    Ok(Json(serde_json::json!({
        "messages": messages,
        "hasMore": has_more,
        "nextAfterSeq": next_after,
    })))
}

pub async fn post_message(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(req): Json<PostMessageReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let content = req
        .ciphertext
        .or(req.content)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "ciphertext or content is required"))?;
    if fetch_session(&state.db, &user.0, &session_id)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .is_none()
    {
        return Err(err(StatusCode::NOT_FOUND, "session not found"));
    }

    // Idempotency-Key header: if the same key has been used to post into
    // this session before, return the existing row instead of creating a
    // duplicate. We piggy-back on `local_id` for storage since happier's
    // schema already uses it as the idempotency anchor.
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);
    let local_id = req.local_id.or_else(|| idem.clone());

    if let Some(ref lid) = local_id {
        let existing: Option<MessageRow> = sqlx::query_as(
            "SELECT id, seq, local_id, sidechain_id, message_role, content, created_at, updated_at
             FROM session_messages WHERE session_id = ?1 AND local_id = ?2",
        )
        .bind(&session_id)
        .bind(lid)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
        if let Some(row) = existing {
            return Ok(Json(serde_json::json!({
                "didWrite": false,
                "message": {
                    "id": row.id,
                    "seq": row.seq,
                    "localId": row.local_id,
                    "createdAt": row.created_at,
                }
            })));
        }
    }

    // Allocate the next seq within this session.
    let next_seq: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM session_messages WHERE session_id = ?1")
            .bind(&session_id)
            .fetch_one(&state.db)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    let id = uuid::Uuid::new_v4().to_string();
    let now = now_secs();
    sqlx::query(
        "INSERT INTO session_messages (id, session_id, seq, local_id, sidechain_id, message_role, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
    )
    .bind(&id)
    .bind(&session_id)
    .bind(next_seq)
    .bind(local_id.as_deref())
    .bind(req.sidechain_id.as_deref())
    .bind(req.message_role.as_deref())
    .bind(&content)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    sqlx::query("UPDATE sessions SET seq = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(next_seq)
        .bind(now)
        .bind(&session_id)
        .execute(&state.db)
        .await
        .ok();

    Ok(Json(serde_json::json!({
        "didWrite": true,
        "message": {
            "id": id,
            "seq": next_seq,
            "localId": local_id,
            "createdAt": now,
        }
    })))
}
