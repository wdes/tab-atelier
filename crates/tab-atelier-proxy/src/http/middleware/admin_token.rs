// SPDX-License-Identifier: MPL-2.0

//! The admin token guard.
//!
//! One shared secret opens the operator API. It is compared in constant time
//! and never echoed, and a refusal says what was wrong without saying what the
//! token is.

use crate::http::middleware::presented;
use crate::server::State;
use crate::transport::{InReq, Reply, json};
use crate::users::constant_time_eq;

/// Whether this request may use `/api/*`.
///
/// # Errors
///
/// A 503 when the installation has no admin token at all — which is a
/// deployment problem, not a credential one — and a 401 when the presented
/// credential is missing or wrong. The 401 names the likely cause, because the
/// alternative is an operator staring at a bare "unauthorized".
pub fn guard(req: &InReq, state: &State) -> Result<(), Reply> {
    if state.admin_token.is_empty() {
        return Err(json(503, r#"{"error":"no admin token configured"}"#));
    }
    let offered = presented(req);
    if constant_time_eq(offered.as_bytes(), state.admin_token.as_bytes()) {
        return Ok(());
    }
    // "admin token required" alone leaves an operator with nothing to act on —
    // the same unhelpful 401 the proxy path deliberately avoids. Say what was
    // wrong without printing anyone's secret.
    let why = if offered.is_empty() {
        "no credential presented — nothing arrived in Authorization: Bearer or x-api-key, \
         which usually means something in front of the proxy stripped it"
    } else if state
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authenticate(&offered)
        .is_some()
    {
        "that is a USER key — it opens the Anthropic path and /me/usage, never this API"
    } else if offered.trim() != offered {
        "the token has leading or trailing whitespace — it was probably pasted with a newline"
    } else {
        "token mismatch — check `tab-atelier-proxy admin-token` ON THE SERVER, as the service user"
    };
    log::warn!("admin: 401 ({} chars presented): {why}", offered.chars().count());
    Err(json(
        401,
        &serde_json::json!({ "error": format!("admin token required: {why}") }).to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::http::middleware::arrival::unknown_peer;

    use super::*;

    fn a_request(bearing: Option<&str>) -> InReq {
        let mut headers = hyper::HeaderMap::new();
        if let Some(v) = bearing {
            headers.insert("x-api-key", v.parse().unwrap());
        }
        InReq {
            method: hyper::Method::GET,
            path: "/api/users".to_owned(),
            query: String::new(),
            headers,
            body: bytes::Bytes::new(),
            peer: unknown_peer(),
        }
    }

    /// The body of a refusal, which every test here inspects.
    fn refusal_body(r: &Reply) -> String {
        match &r.body {
            crate::transport::ReplyBody::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            crate::transport::ReplyBody::Stream(_) => {
                unreachable!("a refusal is never a stream")
            }
        }
    }

    #[test]
    fn the_installation_without_a_token_says_so_rather_than_refusing() {
        let state = State::for_tests(String::new());
        let r = guard(&a_request(Some("anything")), &state).unwrap_err();
        assert_eq!(r.status, 503, "a missing token is a deployment problem");
    }

    #[test]
    fn a_wrong_token_is_refused_without_naming_the_right_one() {
        let state = State::for_tests("s3cret".to_owned());
        let r = guard(&a_request(Some("guess")), &state).unwrap_err();
        assert_eq!(r.status, 401);
        let body = refusal_body(&r);
        assert!(!body.contains("s3cret"), "the refusal must not leak it");
    }

    #[test]
    fn an_absent_credential_is_told_apart_from_a_wrong_one() {
        // The two failures have different fixes, and "it was stripped by a
        // proxy" is not something an operator guesses from "unauthorized".
        let state = State::for_tests("s3cret".to_owned());
        let r = guard(&a_request(None), &state).unwrap_err();
        let body = refusal_body(&r);
        assert!(body.contains("no credential presented"), "{body}");
    }

    #[test]
    fn the_right_token_passes() {
        let state = State::for_tests("s3cret".to_owned());
        assert!(guard(&a_request(Some("s3cret")), &state).is_ok());
    }
}
