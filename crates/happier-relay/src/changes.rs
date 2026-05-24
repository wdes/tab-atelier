// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/v2/changes` + `/v2/cursor`: an in-memory ring of recent entity
//! changes so a happier-mobile client that's been offline can catch
//! up after reconnect.
//!
//! Wire shape mirrors `packages/protocol/src/changes.ts` in upstream
//! happier — each row is `{cursor, kind, entityId, changedAt,
//! hint}`, coalesced by `(kind, entityId)` so rapid updates collapse
//! to a single row at the latest cursor. The mobile UI uses this as
//! "invalidate this entity, re-fetch by id"; it doesn't read the
//! row's content beyond the entity id.
//!
//! Capped at [`CAP`] entries total. When a row is evicted off the
//! front, `floor` advances to that row's cursor — a poll with
//! `after < floor` is answered with `410 cursor-gone` so the client
//! does a full re-snapshot rather than missing changes silently.
//!
//! Single-tenant: we keep one ring for the whole process. The
//! `--shared-account` flag means every device sees the same change
//! stream anyway; sharding by user would be wasted complexity.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::state::AppState;

/// Max rows kept in the ring. ~1 KiB per row; 1024 rows ≈ 1 MiB. At
/// 20 artifact updates/sec (typing) the ring covers ~50 s of recent
/// activity before a client that's been offline longer must
/// re-snapshot via 410.
const CAP: usize = 1024;

/// Hard cap on `limit` per `/v2/changes` call. Matches upstream
/// happier (`apps/server/sources/app/api/routes/changes/
/// changesRoutes.ts`).
const MAX_LIMIT: u32 = 500;
const DEFAULT_LIMIT: u32 = 200;

#[derive(Debug, Clone)]
pub struct ChangeRecord {
    pub kind: &'static str,
    pub entity_id: String,
    pub hint: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct Row {
    cursor: u64,
    kind: &'static str,
    entity_id: String,
    changed_at: i64,
    hint: Option<serde_json::Value>,
}

#[derive(Default)]
struct Inner {
    next_cursor: u64,
    floor: u64,
    rows: std::collections::VecDeque<Row>,
}

#[derive(Clone, Default)]
pub struct ChangesRingBuffer {
    inner: Arc<RwLock<Inner>>,
}

impl ChangesRingBuffer {
    /// Record a change. Coalesces against any existing row with the
    /// same `(kind, entity_id)` — the older row is removed and the
    /// new one appended at the freshly-incremented cursor. Returns
    /// the assigned cursor.
    #[allow(clippy::significant_drop_tightening)] // whole section is the critical region
    pub fn append(&self, ch: ChangeRecord) -> u64 {
        let cur;
        {
            let mut g = self.inner.write();
            g.next_cursor = g.next_cursor.saturating_add(1);
            cur = g.next_cursor;
            g.rows.retain(|r| !(r.kind == ch.kind && r.entity_id == ch.entity_id));
            g.rows.push_back(Row {
                cursor: cur,
                kind: ch.kind,
                entity_id: ch.entity_id,
                changed_at: now_ms(),
                hint: ch.hint,
            });
            while g.rows.len() > CAP {
                if let Some(dropped) = g.rows.pop_front() {
                    g.floor = dropped.cursor;
                }
            }
        }
        cur
    }

    fn snapshot(&self) -> (u64, u64, Vec<Row>) {
        let g = self.inner.read();
        (g.next_cursor, g.floor, g.rows.iter().cloned().collect())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesQuery {
    #[serde(default)]
    pub after: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /v2/cursor` → `{cursor, changesFloor}`. The mobile reads this
/// once after socket connect to learn its starting point.
pub async fn cursor(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (cur, floor, _) = state.changes.snapshot();
    Json(serde_json::json!({ "cursor": cur, "changesFloor": floor }))
}

/// `GET /v2/changes?after=N&limit=M` → `{changes, nextCursor}`.
/// Returns rows with `cursor > after`, capped at `min(limit, 500)`,
/// in cursor-ascending order. `after > cursor` or `after < floor`
/// both produce 410 `{error: "cursor-gone", currentCursor}` so the
/// client knows to drop its cursor and re-snapshot.
pub async fn changes(
    State(state): State<AppState>,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let (current, floor, rows) = state.changes.snapshot();

    if after > current {
        return Err((
            StatusCode::GONE,
            Json(serde_json::json!({ "error": "cursor-gone", "currentCursor": current })),
        ));
    }
    if after > 0 && after < floor {
        return Err((
            StatusCode::GONE,
            Json(serde_json::json!({ "error": "cursor-gone", "currentCursor": current })),
        ));
    }

    let out: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| r.cursor > after)
        .take(limit)
        .map(|r| {
            serde_json::json!({
                "cursor": r.cursor,
                "kind": r.kind,
                "entityId": r.entity_id,
                "changedAt": r.changed_at,
                "hint": r.hint,
            })
        })
        .collect();

    let next_cursor = out
        .last()
        .and_then(|v| v.get("cursor"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(after);
    Ok(Json(serde_json::json!({
        "changes": out,
        "nextCursor": next_cursor,
    })))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
