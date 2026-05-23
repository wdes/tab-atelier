// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/v1/auth` (Ed25519 challenge/response) and the Bearer-token extractor
//! used by every subsequent authenticated route.

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Domain-separation prefix happier embeds when binding a Curve25519
/// content-encryption key to the Ed25519 signing identity. Must match
/// `apps/server/sources/app/encryption/contentKeyBinding.ts` byte for
/// byte (trailing NUL included).
const CONTENT_KEY_BINDING_PREFIX: &[u8] = b"Happy content key v1\0";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequest {
    pub public_key: String,
    pub challenge: String,
    pub signature: String,
    #[serde(default)]
    pub content_public_key: Option<String>,
    #[serde(default)]
    pub content_public_key_sig: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub success: bool,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
}

/// POST `/v1/auth` — Ed25519 challenge/response, returns a JWT.
pub async fn auth_handler(
    State(state): State<AppState>,
    Json(req): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, AuthError> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;

    let public_key = b64.decode(&req.public_key).map_err(|_| AuthError::bad_request("invalid publicKey base64"))?;
    let challenge = b64.decode(&req.challenge).map_err(|_| AuthError::bad_request("invalid challenge base64"))?;
    let signature = b64.decode(&req.signature).map_err(|_| AuthError::bad_request("invalid signature base64"))?;

    if public_key.len() != 32 {
        return Err(AuthError::bad_request("publicKey must be 32 bytes"));
    }
    if challenge.len() != 32 {
        return Err(AuthError::bad_request("challenge must be 32 bytes"));
    }
    if signature.len() != 64 {
        return Err(AuthError::bad_request("signature must be 64 bytes"));
    }

    let pk_array: [u8; 32] = public_key.as_slice().try_into().expect("checked above");
    let verifying_key = VerifyingKey::from_bytes(&pk_array).map_err(|_| AuthError::unauthorized("malformed Ed25519 key"))?;
    let sig_array: [u8; 64] = signature.as_slice().try_into().expect("checked above");
    let signature = Signature::from_bytes(&sig_array);

    verifying_key
        .verify_strict(&challenge, &signature)
        .map_err(|_| AuthError::unauthorized("challenge signature invalid"))?;

    // Content-key binding is optional, but its two halves must travel together.
    let (content_pk, content_pk_sig) = match (req.content_public_key.as_deref(), req.content_public_key_sig.as_deref()) {
        (None, None) => (None, None),
        (Some(_), None) | (None, Some(_)) => {
            return Err(AuthError::bad_request("contentPublicKey and contentPublicKeySig must be provided together"));
        }
        (Some(pk_b64), Some(sig_b64)) => {
            let pk = b64.decode(pk_b64).map_err(|_| AuthError::bad_request("invalid contentPublicKey base64"))?;
            let sig = b64.decode(sig_b64).map_err(|_| AuthError::bad_request("invalid contentPublicKeySig base64"))?;
            if pk.len() != 32 {
                return Err(AuthError::bad_request("contentPublicKey must be 32 bytes"));
            }
            if sig.len() != 64 {
                return Err(AuthError::bad_request("contentPublicKeySig must be 64 bytes"));
            }
            let mut payload = Vec::with_capacity(CONTENT_KEY_BINDING_PREFIX.len() + pk.len());
            payload.extend_from_slice(CONTENT_KEY_BINDING_PREFIX);
            payload.extend_from_slice(&pk);
            let sig_array: [u8; 64] = sig.as_slice().try_into().expect("checked above");
            let content_sig = Signature::from_bytes(&sig_array);
            verifying_key
                .verify_strict(&payload, &content_sig)
                .map_err(|_| AuthError::unauthorized("contentPublicKey binding signature invalid"))?;
            (Some(pk), Some(sig))
        }
    };

    let public_key_hex = hex::encode(&public_key);

    // Single-tenant gate: if an owner is pinned, reject anyone else.
    if let Some(ref owner) = state.owner_pubkey_hex
        && owner != &public_key_hex
    {
        return Err(AuthError::forbidden("only the configured owner can log in"));
    }

    let user_id = if state.shared_account {
        // Map every successful auth onto the single shared account.
        // First device through gets to create it; everyone else just
        // returns the existing id. Per-device key/content-key records
        // are still upserted alongside so the audit trail survives.
        crate::db::upsert_account_shared(
            &state.db,
            &public_key_hex,
            content_pk.as_deref(),
            content_pk_sig.as_deref(),
        )
        .await
    } else {
        crate::db::upsert_account(
            &state.db,
            &public_key_hex,
            content_pk.as_deref(),
            content_pk_sig.as_deref(),
        )
        .await
    }
    .map_err(|e| {
        tracing::warn!(error = ?e, "account upsert failed");
        AuthError::internal()
    })?;

    let token = crate::jwt::issue(&state.jwt_secret, &user_id);
    Ok(Json(AuthResponse { success: true, token }))
}

/// GET `/v1/auth/ping` — confirms the Bearer token is accepted.
pub async fn ping_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Authentication middleware: read `Authorization: Bearer <jwt>`, verify,
/// attach the user id to request extensions. Any handler can then
/// `req.extensions().get::<UserId>()` to identify the caller.
#[derive(Clone, Debug)]
#[allow(dead_code)] // consumed by future authed handlers (sessions, KV, …)
pub struct UserId(pub String);

pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    // Two ways to supply the bearer token:
    //   1. `Authorization: Bearer <jwt>` header — preferred.
    //   2. `?token=<jwt>` query param — needed by browsers using
    //      EventSource (the API can't set headers on SSE connections).
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(std::string::ToString::to_string)
        .or_else(|| {
            req.uri().query().and_then(|q| {
                q.split('&').find_map(|pair| pair.strip_prefix("token=").map(std::string::ToString::to_string))
            })
        });
    let Some(token) = token else {
        return AuthError::unauthorized("missing bearer token (Authorization header or ?token=)")
            .into_response();
    };
    match crate::jwt::verify(&state.jwt_secret, &token) {
        Ok(claims) => {
            req.extensions_mut().insert(UserId(claims.user));
            next.run(req).await
        }
        Err(_) => AuthError::unauthorized("invalid or expired token").into_response(),
    }
}

#[derive(Debug)]
pub struct AuthError {
    status: StatusCode,
    message: String,
}

impl AuthError {
    fn bad_request(msg: &str) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }
    fn unauthorized(msg: &str) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.into() }
    }
    fn forbidden(msg: &str) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: msg.into() }
    }
    fn internal() -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: "internal error".into() }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.message })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::RngCore;

    fn signing_key() -> SigningKey {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn auth_error_constructors_set_status_and_message() {
        let br = AuthError::bad_request("nope");
        assert_eq!(br.status, StatusCode::BAD_REQUEST);
        assert_eq!(br.message, "nope");

        let un = AuthError::unauthorized("who");
        assert_eq!(un.status, StatusCode::UNAUTHORIZED);
        assert_eq!(un.message, "who");

        let fb = AuthError::forbidden("nah");
        assert_eq!(fb.status, StatusCode::FORBIDDEN);
        assert_eq!(fb.message, "nah");

        let it = AuthError::internal();
        assert_eq!(it.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(it.message, "internal error");
    }

    #[test]
    fn auth_error_into_response_preserves_status() {
        let resp = AuthError::forbidden("blocked").into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let ct = resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("application/json"));
    }

    #[test]
    fn auth_request_deserializes_camel_case_required_fields() {
        let raw = r#"{"publicKey":"AAAA","challenge":"BBBB","signature":"CCCC"}"#;
        let req: AuthRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.public_key, "AAAA");
        assert_eq!(req.challenge, "BBBB");
        assert_eq!(req.signature, "CCCC");
        assert!(req.content_public_key.is_none());
        assert!(req.content_public_key_sig.is_none());
    }

    #[test]
    fn auth_request_deserializes_optional_content_key_pair() {
        let raw = r#"{
            "publicKey":"AAAA","challenge":"BBBB","signature":"CCCC",
            "contentPublicKey":"DDDD","contentPublicKeySig":"EEEE"
        }"#;
        let req: AuthRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.content_public_key.as_deref(), Some("DDDD"));
        assert_eq!(req.content_public_key_sig.as_deref(), Some("EEEE"));
    }

    #[test]
    fn auth_request_rejects_snake_case_keys() {
        let raw = r#"{"public_key":"AAAA","challenge":"BBBB","signature":"CCCC"}"#;
        assert!(serde_json::from_str::<AuthRequest>(raw).is_err());
    }

    #[test]
    fn auth_response_serializes_to_expected_shape() {
        let body = serde_json::to_value(AuthResponse {
            success: true,
            token: "jwt.here".into(),
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({"success": true, "token": "jwt.here"}));
    }

    #[test]
    fn error_body_serializes_with_error_field_only() {
        let body = serde_json::to_value(ErrorBody { error: "boom".into() }).unwrap();
        assert_eq!(body, serde_json::json!({"error": "boom"}));
    }

    #[test]
    fn content_key_binding_prefix_matches_typescript_constant() {
        assert_eq!(CONTENT_KEY_BINDING_PREFIX, b"Happy content key v1\0");
        assert_eq!(CONTENT_KEY_BINDING_PREFIX.len(), 21);
    }

    #[test]
    fn good_content_key_binding_signature_verifies() {
        let sk = signing_key();
        let vk = sk.verifying_key();
        let mut content_pk = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut content_pk);

        let mut payload = Vec::with_capacity(CONTENT_KEY_BINDING_PREFIX.len() + 32);
        payload.extend_from_slice(CONTENT_KEY_BINDING_PREFIX);
        payload.extend_from_slice(&content_pk);

        let sig = sk.sign(&payload);
        vk.verify_strict(&payload, &sig).expect("valid binding sig must verify");

        let b64 = base64::engine::general_purpose::STANDARD;
        let sig_b64 = b64.encode(sig.to_bytes());
        let pk_b64 = b64.encode(content_pk);
        assert_eq!(b64.decode(sig_b64).unwrap().len(), 64);
        assert_eq!(b64.decode(pk_b64).unwrap().len(), 32);
    }

    #[test]
    fn tampered_content_key_binding_signature_fails() {
        let sk = signing_key();
        let vk = sk.verifying_key();
        let content_pk = [7u8; 32];

        let mut payload = Vec::new();
        payload.extend_from_slice(CONTENT_KEY_BINDING_PREFIX);
        payload.extend_from_slice(&content_pk);
        let sig = sk.sign(&payload);

        let mut tampered = [7u8; 32];
        tampered[0] ^= 0xff;
        let mut bad_payload = Vec::new();
        bad_payload.extend_from_slice(CONTENT_KEY_BINDING_PREFIX);
        bad_payload.extend_from_slice(&tampered);

        assert!(vk.verify_strict(&bad_payload, &sig).is_err());
    }

    #[test]
    fn wrong_prefix_breaks_binding_verification() {
        let sk = signing_key();
        let vk = sk.verifying_key();
        let content_pk = [9u8; 32];
        let sig = sk.sign(&content_pk);
        let mut payload = Vec::new();
        payload.extend_from_slice(CONTENT_KEY_BINDING_PREFIX);
        payload.extend_from_slice(&content_pk);
        assert!(vk.verify_strict(&payload, &sig).is_err());
    }

    #[test]
    fn user_id_is_constructible_and_clones() {
        let u = UserId("abc".into());
        let cloned = u.clone();
        assert_eq!(cloned.0, "abc");
        assert_eq!(u.0, "abc");
    }
}
