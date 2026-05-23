// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mobile-app pairing-style auth.
//!
//! The happier mobile UI doesn't use the Ed25519 challenge/response
//! flow at `/v1/auth` — that's the desktop CLI's path. The mobile app
//! posts `{publicKey}` to `/v1/auth/account/request` and polls for a
//! state transition `requested` → `authorized`. On an upstream
//! happier server, an already-paired device approves the request out
//! of band (QR code / confirm code).
//!
//! For a single-tenant `--shared-account` relay there is no notion of
//! "another device approves you" — anyone who can reach the relay is
//! the owner by construction. So we short-circuit: every request is
//! authorized immediately, the public key is upserted onto the shared
//! account, and a JWT is returned in the same response.
//!
//! v2 wraps the token in a Curve25519 box so it stays encrypted at
//! rest in case the polling response is logged. We don't implement
//! the box yet — clients that ask for v2 fall back to v1's plaintext
//! token (the JS client in `qrWait.ts` accepts both shapes).

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRequest {
    pub public_key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountResponse {
    pub state: &'static str,
    pub token: String,
    /// The mobile client expects `response` even on the v1 path — it's
    /// the base64-encrypted upstream payload, which we don't generate
    /// for short-circuit auth. Empty string keeps the JSON shape happy.
    pub response: String,
}

/// `POST /v1/auth/account/request` — short-circuit pairing.
pub async fn account_request(
    State(state): State<AppState>,
    Json(req): Json<AccountRequest>,
) -> Result<Json<AccountResponse>, PairingError> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;

    let public_key = b64
        .decode(&req.public_key)
        .map_err(|_| PairingError::bad_request("invalid publicKey base64"))?;
    if public_key.len() != 32 {
        return Err(PairingError::bad_request("publicKey must be 32 bytes"));
    }
    let public_key_hex = hex::encode(&public_key);

    // Mint a JWT for the shared account. Even when `--shared-account`
    // is off this still upserts a per-pubkey account row; the caller
    // gets a token bound to *some* account either way.
    let user_id = if state.shared_account {
        crate::db::upsert_account_shared(&state.db, &public_key_hex, None, None).await
    } else {
        crate::db::upsert_account(&state.db, &public_key_hex, None, None).await
    }
    .map_err(|e| {
        tracing::warn!(error = ?e, "pairing upsert failed");
        PairingError::internal()
    })?;

    let token = crate::jwt::issue(&state.jwt_secret, &user_id);
    Ok(Json(AccountResponse {
        state: "authorized",
        token,
        response: String::new(),
    }))
}

/// `POST /v2/auth/account/request` — same logic, different field name
/// for the encrypted-token shape. Until we implement the box, we ship
/// the token in plaintext and let the client's v2-fallback path pick
/// up `token` (`qrWait.ts` accepts either `tokenEncrypted` or `token`).
pub async fn account_request_v2(
    state: State<AppState>,
    body: Json<AccountRequest>,
) -> Result<Json<AccountResponse>, PairingError> {
    account_request(state, body).await
}

#[derive(Debug)]
pub struct PairingError {
    status: StatusCode,
    message: String,
}

impl PairingError {
    fn bad_request(msg: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".into(),
        }
    }
}

impl IntoResponse for PairingError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}
