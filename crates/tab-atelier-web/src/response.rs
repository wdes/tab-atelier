// SPDX-License-Identifier: MPL-2.0

//! The response type both UIs answer with, and the token gate they both need.

use bytes::Bytes;
use http::{HeaderValue, Response, StatusCode, header};
use http_body_util::Full;

/// A response body: always a complete in-memory buffer. Neither UI streams a
/// body of its own — a relayed one, if a crate needs it, is that crate's
/// business and not this shared layer's.
pub type Body = Full<Bytes>;

/// A response of bytes, with a content type.
#[must_use]
pub fn bytes(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// A JSON response.
#[must_use]
pub fn json(status: StatusCode, value: &serde_json::Value) -> Response<Body> {
    bytes(status, "application/json", value.to_string())
}

/// A JSON error, `{"error": message}` — the one error shape both UIs use.
#[must_use]
pub fn error(status: StatusCode, message: &str) -> Response<Body> {
    json(status, &serde_json::json!({ "error": message }))
}

/// A 302 redirect to `to`.
#[must_use]
pub fn redirect(to: &str) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::FOUND;
    if let Ok(value) = HeaderValue::from_str(to) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

/// Whether a request holds the expected token — in `Authorization: Bearer`, or
/// in a `?token=` query.
///
/// The query is accepted because not every caller can set a header: a plain
/// `<a href>`, an operator pasting a URL. Both are compared in constant time, so
/// the check does not leak how much of a guessed token was right.
///
/// A `None` expected token authorizes **nobody**: a server with no token
/// configured fails closed rather than standing open.
#[must_use]
pub fn token_matches(expected: Option<&str>, authorization: Option<&HeaderValue>, query_token: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let from_header = authorization
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let provided = from_header.or(query_token);
    provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
}

/// The `token` value of a query string, if any.
#[must_use]
pub fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "token")
        .map(|(_, value)| value.to_string())
}

/// Compares without leaking where two strings first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, error, json, query_token, redirect, token_matches};
    use http::{HeaderValue, StatusCode};

    /// The gate is exact and fail-closed: the right token passes (header OR
    /// query), a wrong one does not, and a server with no token lets nobody in.
    #[test]
    fn the_token_gate_is_exact_and_fails_closed() {
        let bearer = |t: &str| HeaderValue::from_str(&format!("Bearer {t}")).expect("header");
        assert!(token_matches(Some("s3cret"), Some(&bearer("s3cret")), None));
        assert!(
            token_matches(Some("s3cret"), None, Some("s3cret")),
            "query token accepted"
        );
        assert!(!token_matches(Some("s3cret"), Some(&bearer("wrong")), None));
        assert!(!token_matches(Some("s3cret"), None, Some("wrong")));
        assert!(
            !token_matches(Some("s3cret"), Some(&HeaderValue::from_static("s3cret")), None),
            "the scheme is required"
        );
        assert!(!token_matches(None, None, Some("anything")), "no token = nobody in");
    }

    /// The helpers build the shapes both UIs depend on.
    #[test]
    fn responses_carry_the_expected_shape() {
        let j = json(StatusCode::OK, &serde_json::json!({ "ok": true }));
        assert_eq!(j.status(), StatusCode::OK);
        assert_eq!(j.headers()["content-type"], "application/json");

        let e = error(StatusCode::BAD_REQUEST, "nope");
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);

        let r = redirect("/tabs/by-id/x/view?token=t");
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers()["location"], "/tabs/by-id/x/view?token=t");
    }

    #[test]
    fn the_query_token_is_extracted() {
        assert_eq!(query_token(Some("a=1&token=abc&b=2")).as_deref(), Some("abc"));
        assert_eq!(query_token(Some("a=1")), None);
        assert_eq!(query_token(None), None);
    }

    #[test]
    fn constant_time_equality_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
