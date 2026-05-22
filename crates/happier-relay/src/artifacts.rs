// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Artifacts: per-user opaque encrypted blob store. Header and body
//! are stored as `BLOB` bytes; clients send/receive base64 over HTTP.
//! Independent version counters per field — typical use case is the
//! header changing often (filename, timestamps) without re-uploading
//! a large body.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::auth::UserId;
use crate::state::{AppState, BroadcastMsg};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactView {
    pub id: String,
    pub header: String,
    pub header_version: i64,
    pub body: String,
    pub body_version: i64,
    pub data_encryption_key: String,
    pub seq: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSummary {
    pub id: String,
    pub header: String,
    pub header_version: i64,
    pub data_encryption_key: String,
    pub seq: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReq {
    pub id: String,
    pub header: String,
    pub body: String,
    pub data_encryption_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReq {
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub expected_header_version: Option<i64>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub expected_body_version: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct ArtifactRow {
    id: String,
    header: Vec<u8>,
    header_version: i64,
    body: Vec<u8>,
    body_version: i64,
    data_encryption_key: Vec<u8>,
    seq: i64,
    created_at: i64,
    updated_at: i64,
}

impl ArtifactRow {
    fn into_view(self) -> ArtifactView {
        ArtifactView {
            id: self.id,
            header: B64.encode(&self.header),
            header_version: self.header_version,
            body: B64.encode(&self.body),
            body_version: self.body_version,
            data_encryption_key: B64.encode(&self.data_encryption_key),
            seq: self.seq,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn into_summary(self) -> ArtifactSummary {
        ArtifactSummary {
            id: self.id,
            header: B64.encode(&self.header),
            header_version: self.header_version,
            data_encryption_key: B64.encode(&self.data_encryption_key),
            seq: self.seq,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed())
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

fn decode_b64(field: &str, value: &str) -> Result<Vec<u8>, (StatusCode, Json<serde_json::Value>)> {
    B64.decode(value).map_err(|_| err(StatusCode::BAD_REQUEST, &format!("invalid base64 in {field}")))
}

// --- handlers ---------------------------------------------------------------

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let rows: Vec<ArtifactRow> = sqlx::query_as(
        "SELECT id, header, header_version, body, body_version, data_encryption_key, seq, created_at, updated_at
         FROM artifacts WHERE account_id = ?1 ORDER BY updated_at DESC",
    )
    .bind(&user.0)
    .fetch_all(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let summaries: Vec<ArtifactSummary> = rows.into_iter().map(ArtifactRow::into_summary).collect();
    Ok(Json(serde_json::to_value(summaries).unwrap()))
}

pub async fn get_one(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let row: Option<ArtifactRow> = sqlx::query_as(
        "SELECT id, header, header_version, body, body_version, data_encryption_key, seq, created_at, updated_at
         FROM artifacts WHERE id = ?1 AND account_id = ?2",
    )
    .bind(&id)
    .bind(&user.0)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    row.map_or_else(
        || Err(err(StatusCode::NOT_FOUND, "artifact not found")),
        |r| Ok(Json(serde_json::to_value(r.into_view()).unwrap())),
    )
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(req): Json<CreateReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let header = decode_b64("header", &req.header)?;
    let body = decode_b64("body", &req.body)?;
    let dek = decode_b64("dataEncryptionKey", &req.data_encryption_key)?;
    let now = now_secs();

    // Compute the next seq for this user. happier uses a global event
    // sequence; we keep it per-user since that's enough for client diff.
    let next_seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM artifacts WHERE account_id = ?1")
        .bind(&user.0)
        .fetch_one(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    let insert = sqlx::query(
        "INSERT INTO artifacts (id, account_id, header, header_version, body, body_version, data_encryption_key, seq, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, ?4, 1, ?5, ?6, ?7, ?7)",
    )
    .bind(&req.id)
    .bind(&user.0)
    .bind(&header)
    .bind(&body)
    .bind(&dek)
    .bind(next_seq)
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(e) = insert {
        if matches!(&e, sqlx::Error::Database(db) if db.is_unique_violation()) {
            return Err(err(StatusCode::CONFLICT, "artifact with this id already exists"));
        }
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"));
    }

    let view = ArtifactView {
        id: req.id.clone(),
        header: req.header,
        header_version: 1,
        body: req.body,
        body_version: 1,
        data_encryption_key: req.data_encryption_key,
        seq: next_seq,
        created_at: now,
        updated_at: now,
    };
    fanout(&state.broadcast_tx, &user.0, "artifact-create", &view);
    Ok(Json(serde_json::to_value(&view).unwrap()))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Pull current state so we can CAS both fields atomically.
    let row: Option<ArtifactRow> = sqlx::query_as(
        "SELECT id, header, header_version, body, body_version, data_encryption_key, seq, created_at, updated_at
         FROM artifacts WHERE id = ?1 AND account_id = ?2",
    )
    .bind(&id)
    .bind(&user.0)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let mut row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "artifact not found"))?;

    if req.header.is_some() && req.expected_header_version.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "expectedHeaderVersion required when updating header"));
    }
    if req.body.is_some() && req.expected_body_version.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "expectedBodyVersion required when updating body"));
    }

    let header_mismatch = req
        .expected_header_version
        .is_some_and(|v| v != row.header_version);
    let body_mismatch = req.expected_body_version.is_some_and(|v| v != row.body_version);
    if header_mismatch || body_mismatch {
        return Ok(Json(serde_json::json!({
            "success": false,
            "error": "version-mismatch",
            "currentHeaderVersion": row.header_version,
            "currentHeader": B64.encode(&row.header),
            "currentBodyVersion": row.body_version,
            "currentBody": B64.encode(&row.body),
        })));
    }

    if let Some(ref h) = req.header {
        row.header = decode_b64("header", h)?;
        row.header_version += 1;
    }
    if let Some(ref b) = req.body {
        row.body = decode_b64("body", b)?;
        row.body_version += 1;
    }
    let now = now_secs();
    sqlx::query(
        "UPDATE artifacts SET header = ?1, header_version = ?2, body = ?3, body_version = ?4, updated_at = ?5 WHERE id = ?6",
    )
    .bind(&row.header)
    .bind(row.header_version)
    .bind(&row.body)
    .bind(row.body_version)
    .bind(now)
    .bind(&row.id)
    .execute(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    let resp = serde_json::json!({
        "success": true,
        "headerVersion": row.header_version,
        "bodyVersion": row.body_version,
    });
    fanout(
        &state.broadcast_tx,
        &user.0,
        "artifact-update",
        &serde_json::json!({
            "id": row.id,
            "headerVersion": row.header_version,
            "bodyVersion": row.body_version,
        }),
    );
    Ok(Json(resp))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendReq {
    pub expected_body_version: i64,
    /// Raw bytes (base64) to concatenate onto the existing body.
    pub suffix: String,
}

/// Append-only update path. Mirrors the regular CAS update but skips
/// the "upload the full body every tick" tax that dominates bandwidth
/// for terminal-scrollback artifacts (which are 99 %% just-appended).
/// Header is untouched; only the body field grows + its version bumps.
pub async fn append(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(req): Json<AppendReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let suffix = decode_b64("suffix", &req.suffix)?;
    if suffix.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "suffix is empty"));
    }
    let row: Option<ArtifactRow> = sqlx::query_as(
        "SELECT id, header, header_version, body, body_version, data_encryption_key, seq, created_at, updated_at
         FROM artifacts WHERE id = ?1 AND account_id = ?2",
    )
    .bind(&id)
    .bind(&user.0)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let mut row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "artifact not found"))?;

    if req.expected_body_version != row.body_version {
        return Ok(Json(serde_json::json!({
            "success": false,
            "error": "version-mismatch",
            "currentBodyVersion": row.body_version,
            // Returning the current body lets the client rebase + retry
            // without a separate GET round-trip. Matches the
            // version-mismatch shape used by the regular update path.
            "currentBody": B64.encode(&row.body),
        })));
    }

    row.body.extend_from_slice(&suffix);
    row.body_version += 1;
    let now = now_secs();
    sqlx::query("UPDATE artifacts SET body = ?1, body_version = ?2, updated_at = ?3 WHERE id = ?4")
        .bind(&row.body)
        .bind(row.body_version)
        .bind(now)
        .bind(&row.id)
        .execute(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

    let resp = serde_json::json!({
        "success": true,
        "bodyVersion": row.body_version,
        "bodyLen": row.body.len(),
    });
    fanout(
        &state.broadcast_tx,
        &user.0,
        "artifact-update",
        &serde_json::json!({
            "id": row.id,
            "headerVersion": row.header_version,
            "bodyVersion": row.body_version,
        }),
    );
    Ok(Json(resp))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = sqlx::query("DELETE FROM artifacts WHERE id = ?1 AND account_id = ?2")
        .bind(&id)
        .bind(&user.0)
        .execute(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    if result.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "artifact not found"));
    }
    fanout(&state.broadcast_tx, &user.0, "artifact-delete", &serde_json::json!({ "id": id }));
    Ok(Json(serde_json::json!({ "success": true })))
}

/// Drop a broadcast request into the fan-out channel. `send` returns
/// the count of receivers; zero is normal (no devices subscribed) and
/// not worth logging.
fn fanout<T: serde::Serialize>(tx: &broadcast::Sender<BroadcastMsg>, user_id: &str, event: &str, payload: &T) {
    let body = match serde_json::to_value(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, event, user = user_id, "fanout: serialize failed");
            return;
        }
    };
    let _ = tx.send(BroadcastMsg {
        user_id: user_id.to_string(),
        event: event.to_string(),
        payload: body,
    });
}
