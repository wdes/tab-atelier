// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mobile-to-desktop keystroke relay.
//!
//! POST `/v1/tab-input { tabName, bytes }` enqueues a keystroke event
//! for the user's connected machines. GET `/v1/tab-input/pending?since=N`
//! drains everything with seq > N, **long-polling** for up to
//! `wait_ms` (capped to 30 s) when the queue is empty so the desktop's
//! input flush is sub-second responsive without polling burn.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Extension, Query, State},
    http::StatusCode,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use crate::auth::UserId;
use crate::state::AppState;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const MAX_WAIT: Duration = Duration::from_secs(30);

/// Per-user wake-up signal. The long-poll handler waits on this and
/// the POST handler fires it whenever a row lands.
#[derive(Default, Clone)]
pub struct InputNotifier {
    inner: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl InputNotifier {
    pub async fn notify_user(&self, user_id: &str) {
        let entry = {
            let mut map = self.inner.lock().await;
            Arc::clone(map.entry(user_id.to_string()).or_default())
        };
        entry.notify_waiters();
    }

    async fn wait_for_user(&self, user_id: &str) -> Arc<Notify> {
        let mut map = self.inner.lock().await;
        Arc::clone(map.entry(user_id.to_string()).or_default())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostInputReq {
    /// Human-readable tab name as defined by tab-atelier. The bridge
    /// side looks it up against its current `TabSnapshot`.
    pub tab_name: String,
    /// Base64-encoded raw bytes to feed to the PTY. Use this for both
    /// printable characters and control codes (ctrl-c = 0x03, etc).
    pub bytes: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInputEvent {
    pub seq: i64,
    pub tab_name: String,
    pub bytes: String,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    #[serde(default)]
    pub since: Option<i64>,
    /// Milliseconds the server may hold the response open waiting for
    /// new input. Capped server-side at [`MAX_WAIT`].
    #[serde(default, rename = "waitMs")]
    pub wait_ms: Option<u64>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed())
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

pub async fn post_input(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(req): Json<PostInputReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if req.tab_name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "tabName required"));
    }
    let bytes = B64
        .decode(&req.bytes)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "invalid base64 in bytes"))?;
    if bytes.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty bytes"));
    }
    let now = now_secs();
    let res = sqlx::query("INSERT INTO tab_input (account_id, tab_name, bytes, created_at) VALUES (?1, ?2, ?3, ?4)")
        .bind(&user.0)
        .bind(&req.tab_name)
        .bind(&bytes)
        .bind(now)
        .execute(&state.db)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    let seq = res.last_insert_rowid();
    // Wake any long-poller waiting on this user's queue.
    state.input_notifier.notify_user(&user.0).await;
    Ok(Json(serde_json::json!({ "seq": seq })))
}

pub async fn pending(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Query(q): Query<PendingQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let since = q.since.unwrap_or(0);
    let wait = q
        .wait_ms
        .map_or(Duration::from_secs(0), Duration::from_millis)
        .min(MAX_WAIT);

    // First pass — drain anything already queued.
    let rows = fetch_pending(&state, &user.0, since).await?;
    if !rows.is_empty() || wait.is_zero() {
        return Ok(Json(serde_json::json!({
            "events": rows,
            "highestSeq": rows.last().map_or(since, |r| r.seq),
        })));
    }

    // Empty + caller asked us to wait. Subscribe to the user's wake
    // channel and re-query when either the channel fires or the
    // timeout elapses.
    let notify = state.input_notifier.wait_for_user(&user.0).await;
    let _ = tokio::time::timeout(wait, notify.notified()).await;
    let rows = fetch_pending(&state, &user.0, since).await?;
    Ok(Json(serde_json::json!({
        "events": rows,
        "highestSeq": rows.last().map_or(since, |r| r.seq),
    })))
}

async fn fetch_pending(
    state: &AppState,
    user_id: &str,
    since: i64,
) -> Result<Vec<PendingInputEvent>, (StatusCode, Json<serde_json::Value>)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        seq: i64,
        tab_name: String,
        bytes: Vec<u8>,
        created_at: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT seq, tab_name, bytes, created_at FROM tab_input
         WHERE account_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT 500",
    )
    .bind(user_id)
    .bind(since)
    .fetch_all(&state.db)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    Ok(rows
        .into_iter()
        .map(|r| PendingInputEvent {
            seq: r.seq,
            tab_name: r.tab_name,
            bytes: B64.encode(&r.bytes),
            created_at: r.created_at,
        })
        .collect())
}
