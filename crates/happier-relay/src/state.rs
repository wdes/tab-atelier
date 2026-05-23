// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use socketioxide::SocketIo;
use tokio::sync::broadcast;

/// One fan-out request from an HTTP handler. Sent into a broadcast
/// channel so both the socket.io loop (which echoes it to connected
/// devices) and the SSE handler (which pipes it to browser tabs) see
/// every event without competing for the receiver.
#[derive(Debug, Clone)]
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
    /// When true, every authed device shares the same `account_id`
    /// (the first one created). Used for the local-LAN self-host
    /// case where tab-atelier + web UI need to see the same artifacts.
    pub shared_account: bool,
    /// Stable, human-readable host identifier surfaced as the
    /// machine `id` in `GET /v1/machines`. The mobile UI's
    /// `getMachineDisplayName` falls back to this when (as in our
    /// case) the encrypted `metadata` is empty, so it's effectively
    /// the displayed machine name. Sourced from a file at startup;
    /// persisted across restarts.
    pub machine_id: String,
    /// Multi-consumer broadcast channel for relay-wide events.
    /// HTTP handlers send into it; the socket.io loop + every SSE
    /// stream subscribe. Capacity matters only for slow consumers,
    /// which on overflow get a `RecvError::Lagged` and recover by
    /// re-fetching the resource.
    pub broadcast_tx: broadcast::Sender<BroadcastMsg>,
    /// Per-user wake-up channel for the `/v1/tab-input/pending`
    /// long-poll. POST handlers tap this so waiters return immediately
    /// instead of waiting out the full timeout.
    pub input_notifier: crate::tab_input::InputNotifier,
}

/// Background task that owns the `SocketIo` handle and forwards each
/// broadcast event into the right room. Returns when the channel
/// closes (i.e. on process shutdown).
pub async fn broadcast_loop(io: SocketIo, mut rx: broadcast::Receiver<BroadcastMsg>) {
    loop {
        match rx.recv().await {
            Ok(msg) => {
                let room = crate::socket::room_for_user(&msg.user_id);
                if let Err(e) = io.to(room).emit(&msg.event, &msg.payload).await {
                    tracing::warn!(error = ?e, event = msg.event, user = msg.user_id, "fanout failed");
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "broadcast_loop lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
