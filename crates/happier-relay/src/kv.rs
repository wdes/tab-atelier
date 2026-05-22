// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-user KV store. Opaque base64-encoded values, optimistic version
//! semantics matching happier's `UserKVStore`.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::auth::UserId;
use crate::state::AppState;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, Serialize)]
pub struct KvEntry {
    pub key: String,
    pub value: Option<String>,
    pub version: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct BulkGetReq {
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Mutation {
    pub key: String,
    /// `null` means "delete this key". Anything else is a base64 blob.
    /// Server doesn't decode it — we just store the bytes for the
    /// client to retrieve later.
    #[serde(default)]
    pub value: Option<String>,
    /// `-1` is the sentinel happier uses for "I'm creating a brand-new
    /// key; reject if one already exists". Any other value is the
    /// concrete version the client believes is current.
    pub version: i64,
}

#[derive(Debug, Deserialize)]
pub struct MutateReq {
    pub mutations: Vec<Mutation>,
}

#[derive(sqlx::FromRow)]
struct KvRow {
    key: String,
    value: Option<Vec<u8>>,
    version: i64,
}

impl KvRow {
    fn into_entry(self) -> KvEntry {
        KvEntry {
            key: self.key,
            value: self.value.as_deref().map(|b| B64.encode(b)),
            version: self.version,
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

pub async fn get_one(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let row: Option<KvRow> =
        sqlx::query_as("SELECT key, value, version FROM user_kv WHERE account_id = ?1 AND key = ?2")
            .bind(&user.0)
            .bind(&key)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    row.map_or_else(
        || Err(err(StatusCode::NOT_FOUND, "key not found")),
        |r| Ok(Json(serde_json::to_value(r.into_entry()).unwrap())),
    )
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    // SQLite's LIKE with `LIKE 'prefix%' ESCAPE '\'` is fine here — the
    // prefix is bounded by `limit` and the index covers (account_id, key).
    let pattern = q.prefix.as_deref().map(|p| format!("{p}%"));
    let rows: Vec<KvRow> = if let Some(pat) = pattern {
        sqlx::query_as("SELECT key, value, version FROM user_kv WHERE account_id = ?1 AND key LIKE ?2 ORDER BY key ASC LIMIT ?3")
            .bind(&user.0)
            .bind(pat)
            .bind(limit)
            .fetch_all(&state.db)
            .await
    } else {
        sqlx::query_as("SELECT key, value, version FROM user_kv WHERE account_id = ?1 ORDER BY key ASC LIMIT ?2")
            .bind(&user.0)
            .bind(limit)
            .fetch_all(&state.db)
            .await
    }
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let items: Vec<KvEntry> = rows.into_iter().map(KvRow::into_entry).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn bulk_get(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(req): Json<BulkGetReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if req.keys.is_empty() || req.keys.len() > 100 {
        return Err(err(StatusCode::BAD_REQUEST, "keys must contain between 1 and 100 entries"));
    }
    // We don't trust input strings for SQL building, so use parameter
    // binding via repeated query. For 100 keys that's still fast
    // enough; switching to a single IN(...) generated query is a later
    // optimisation if it ever shows up in a profile.
    let mut values = Vec::with_capacity(req.keys.len());
    for key in &req.keys {
        let row: Option<KvRow> =
            sqlx::query_as("SELECT key, value, version FROM user_kv WHERE account_id = ?1 AND key = ?2")
                .bind(&user.0)
                .bind(key)
                .fetch_optional(&state.db)
                .await
                .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
        if let Some(r) = row {
            values.push(r.into_entry());
        }
    }
    Ok(Json(serde_json::json!({ "values": values })))
}

pub async fn mutate(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(req): Json<MutateReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if req.mutations.is_empty() || req.mutations.len() > 100 {
        return Err(err(StatusCode::BAD_REQUEST, "mutations must contain between 1 and 100 entries"));
    }

    // Pre-validate all base64 payloads so we either accept the whole
    // batch or reject it — happier's behaviour for bulk mutate is
    // partial-success with per-key errors, but the simpler all-or-
    // nothing is fine for the spike.
    let mut decoded: Vec<(String, Option<Vec<u8>>, i64)> = Vec::with_capacity(req.mutations.len());
    for m in req.mutations {
        let bytes = match m.value.as_deref() {
            None => None,
            Some(s) => Some(B64.decode(s).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid base64 in mutation value"))?),
        };
        decoded.push((m.key, bytes, m.version));
    }

    let mut tx = state.db.begin().await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let now = now_secs();
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(decoded.len());
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for (key, value_bytes, expected) in decoded {
        let existing: Option<(i64,)> = sqlx::query_as("SELECT version FROM user_kv WHERE account_id = ?1 AND key = ?2")
            .bind(&user.0)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;

        match (existing, expected) {
            // Existing key, version matches → update or delete.
            (Some((cur_v,)), v) if v == cur_v => {
                if value_bytes.is_none() {
                    sqlx::query("DELETE FROM user_kv WHERE account_id = ?1 AND key = ?2")
                        .bind(&user.0)
                        .bind(&key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
                } else {
                    sqlx::query("UPDATE user_kv SET value = ?1, version = version + 1, updated_at = ?2 WHERE account_id = ?3 AND key = ?4")
                        .bind(value_bytes.as_deref())
                        .bind(now)
                        .bind(&user.0)
                        .bind(&key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
                }
                results.push(serde_json::json!({ "key": key, "version": cur_v + 1 }));
            }
            // No existing key, client asked for create (version == -1).
            (None, -1) => {
                if value_bytes.is_none() {
                    // Deleting a non-existent key is a no-op success.
                    results.push(serde_json::json!({ "key": key, "version": 0 }));
                } else {
                    sqlx::query("INSERT INTO user_kv (account_id, key, value, version, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?4)")
                        .bind(&user.0)
                        .bind(&key)
                        .bind(value_bytes.as_deref())
                        .bind(now)
                        .execute(&mut *tx)
                        .await
                        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
                    results.push(serde_json::json!({ "key": key, "version": 1 }));
                }
            }
            // Version mismatch — surface the current state.
            (Some((cur_v,)), _) => {
                let cur_val: Option<Vec<u8>> = sqlx::query_scalar("SELECT value FROM user_kv WHERE account_id = ?1 AND key = ?2")
                    .bind(&user.0)
                    .bind(&key)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
                errors.push(serde_json::json!({
                    "key": key,
                    "error": "version-mismatch",
                    "version": cur_v,
                    "value": cur_val.as_deref().map(|b| B64.encode(b)),
                }));
            }
            (None, _) => {
                errors.push(serde_json::json!({
                    "key": key,
                    "error": "version-mismatch",
                    "version": 0,
                    "value": serde_json::Value::Null,
                }));
            }
        }
    }

    if errors.is_empty() {
        tx.commit().await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
        Ok(Json(serde_json::json!({ "success": true, "results": results })))
    } else {
        // Roll back so partial successes don't sneak in. Clients re-fetch.
        let _ = tx.rollback().await;
        Ok(Json(serde_json::json!({ "success": false, "errors": errors })))
    }
}
