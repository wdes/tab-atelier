// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use socketioxide::SocketIo;
use tokio::sync::mpsc;

/// One fan-out request from an HTTP handler to the socket.io broadcast
/// loop. The loop runs on a dedicated tokio task that owns `SocketIo`,
/// sidestepping the Send-future constraint on axum handlers.
#[derive(Debug)]
pub struct BroadcastMsg {
    pub user_id: String,
    pub event: String,
    pub payload: serde_json::Value,
}

/// Shared application state handed to every axum handler. Cheap to
/// clone (everything inside is an Arc / Pool / channel sender).
#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::SqlitePool,
    /// Symmetric secret used to sign JWTs. Derived once from the
    /// `--master-secret` flag at startup; never logged.
    pub jwt_secret: Arc<Vec<u8>>,
    /// When set, only accept `/v1/auth` from this exact hex-encoded
    /// Ed25519 public key. None = accept-and-pin on first login.
    pub owner_pubkey_hex: Option<String>,
    /// Send into this channel to fan out a socket.io event to a user's
    /// connected devices. Best-effort: a full or closed channel just
    /// drops the message (HTTP response is already committed).
    pub broadcast_tx: mpsc::UnboundedSender<BroadcastMsg>,
    /// Per-user wake-up channel for the `/v1/tab-input/pending`
    /// long-poll. POST handlers tap this so waiters return immediately
    /// instead of waiting out the full timeout.
    pub input_notifier: crate::tab_input::InputNotifier,
}

/// Background task that owns the `SocketIo` handle and forwards each
/// `BroadcastMsg` into the right room. Returns when `rx` closes.
pub async fn broadcast_loop(io: SocketIo, mut rx: mpsc::UnboundedReceiver<BroadcastMsg>) {
    while let Some(msg) = rx.recv().await {
        let room = crate::socket::room_for_user(&msg.user_id);
        if let Err(e) = io.to(room).emit(&msg.event, &msg.payload).await {
            tracing::warn!(error = ?e, event = msg.event, user = msg.user_id, "fanout failed");
        }
    }
}
