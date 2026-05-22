// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! socket.io v4 baseline.
//!
//! Each socket is authenticated at handshake time via a JWT carried in
//! the `auth` payload (`{ token: "..." }`). On success the socket joins
//! the user's room (named `user:<user_id>`) so subsequent
//! machine/artifact updates fan out to every other device the same user
//! has online. On failure we disconnect immediately — same shape as
//! happier's `validateCurrentMachineSocket()` pattern.

use serde::{Deserialize, Serialize};
use socketioxide::extract::{AckSender, Data, SocketRef, State};

use crate::state::AppState;

/// Per-socket extension storing the authenticated user id. Inserted
/// during the connect handler so per-event handlers can read it back
/// without re-verifying the JWT each time.
#[derive(Clone, Debug)]
pub struct AuthedUser(pub String);

/// Payload shape the client sends in `io.connect({ auth: { token } })`.
#[derive(Debug, Deserialize)]
pub struct AuthPayload {
    pub token: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MachineUpdate {
    /// Machine id the update is about. Sender includes it so peers
    /// can route or filter; the server doesn't otherwise inspect it.
    #[serde(rename = "machineId")]
    pub machine_id: String,
    /// Opaque encrypted metadata blob. Server stores nothing — purely a relay.
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ArtifactUpdate {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub payload: serde_json::Value,
}

#[must_use]
pub fn room_for_user(user_id: &str) -> String {
    format!("user:{user_id}")
}

/// Register all socket-side handlers on the default namespace.
///
/// The connect handler is async because we touch the shared state
/// (JWT secret); event handlers are sync where possible to keep the
/// hot path lean.
pub async fn on_connect(socket: SocketRef, Data(auth): Data<AuthPayload>, State(state): State<AppState>) {
    match crate::jwt::verify(&state.jwt_secret, &auth.token) {
        Ok(claims) => {
            tracing::info!(user = %claims.user, sid = ?socket.id, "socket authed");
            let room = room_for_user(&claims.user);
            // `Socket::join` is infallible (the local-adapter variant);
            // the operator-on-room version that returns Result is for
            // remote ops. Either way: nothing to handle here.
            socket.join(room);
            socket.extensions.insert(AuthedUser(claims.user));
            socket.on("ping", ping_handler);
            socket.on("machine-update", machine_update_handler);
            socket.on("artifact-update", artifact_update_handler);
        }
        Err(e) => {
            tracing::warn!(error = ?e, "socket auth rejected");
            // Emit a connect-error-ish event so clients can surface the
            // reason, then drop the connection. socketioxide doesn't
            // have a `connect_error` primitive, so a one-shot `error`
            // event + disconnect is the idiomatic substitute.
            let _ = socket.emit("error", &serde_json::json!({ "error": "invalid token" }));
            let _ = socket.disconnect();
        }
    }
}

/// `ping` — client sends nothing, server acks `{ ok: true, ts }`. Used
/// by clients to keep the connection live and as a health probe.
async fn ping_handler(socket: SocketRef, ack: AckSender) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_u64, |d| d.as_secs());
    let _ = ack.send(&serde_json::json!({ "ok": true, "ts": ts }));
    tracing::debug!(sid = ?socket.id, "ping handled");
}

/// `machine-update` — relay to the rest of the user's devices.
async fn machine_update_handler(socket: SocketRef, Data(update): Data<MachineUpdate>) {
    let Some(user) = socket.extensions.get::<AuthedUser>() else {
        tracing::warn!(sid = ?socket.id, "machine-update from unauthed socket");
        return;
    };
    let room = room_for_user(&user.0);
    if let Err(e) = socket.to(room).emit("machine-update", &update).await {
        tracing::warn!(error = ?e, "machine-update broadcast failed");
    }
}

/// `artifact-update` — relay to the rest of the user's devices.
async fn artifact_update_handler(socket: SocketRef, Data(update): Data<ArtifactUpdate>) {
    let Some(user) = socket.extensions.get::<AuthedUser>() else {
        tracing::warn!(sid = ?socket.id, "artifact-update from unauthed socket");
        return;
    };
    let room = room_for_user(&user.0);
    if let Err(e) = socket.to(room).emit("artifact-update", &update).await {
        tracing::warn!(error = ?e, "artifact-update broadcast failed");
    }
}
