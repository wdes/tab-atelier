// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's own panes: the board of decisions, and the two rulings on them.
//!
//! Everything here is behind the token. A kiosk with no token answers 401
//! rather than serving the board — the panes are an operator's view, and the
//! absence of a token is not a licence to read them.

use http::{HeaderValue, Response, StatusCode, header};

use crate::body::{BoxBody, full, json_response};
use crate::decisions::{DecisionError, Journal};

/// The panes' state: the journal, and the token that opens it.
#[derive(Debug, Clone)]
pub struct Panes {
    journal: Journal,
    token: Option<String>,
}

impl Panes {
    pub const fn new(journal: Journal, token: Option<String>) -> Self {
        Self { journal, token }
    }

    /// Whether a request's `Authorization` header carries the pane token.
    ///
    /// No token configured → nobody is authorized, so the panes fail closed
    /// instead of standing open.
    pub fn authorized(&self, authorization: Option<&HeaderValue>) -> bool {
        let Some(expected) = self.token.as_deref() else {
            return false;
        };
        let Some(provided) = authorization
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }

    /// `GET /decisions` — the board.
    pub fn list(&self, include_archived: bool) -> Response<BoxBody> {
        self.journal.list(include_archived).map_or_else(
            |_| unavailable(),
            |decisions| json_response(StatusCode::OK, &serde_json::json!({ "decisions": decisions })),
        )
    }

    /// `GET /decisions/file?path=` — one linked file, from inside the outbox.
    pub fn file(&self, path: &str) -> Response<BoxBody> {
        match self.journal.read_file(path) {
            Ok((bytes, content_type)) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(full(bytes))
                .expect("static response builder"),
            Err(error) => decision_error(&error),
        }
    }

    /// `POST /decisions/<id>/read` — mark read.
    pub fn read(&self, id: &str) -> Response<BoxBody> {
        match self.journal.mark_read(id) {
            Ok(decision) => json_response(StatusCode::OK, &serde_json::json!({ "decision": decision })),
            Err(error) => decision_error(&error),
        }
    }

    /// `POST /decisions/<id>/tranch` — rule, with a verdict.
    pub fn tranch(&self, id: &str, verdict: &str) -> Response<BoxBody> {
        match self.journal.tranch(id, verdict.trim()) {
            Ok(decision) => json_response(StatusCode::OK, &serde_json::json!({ "decision": decision })),
            Err(error) => decision_error(&error),
        }
    }

    /// The 401 for a request without the token.
    pub fn unauthorized() -> Response<BoxBody> {
        json_response(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "error": "unauthorized" }),
        )
    }

    /// The 400 for a body that is not the JSON the pane expects.
    pub fn bad_request() -> Response<BoxBody> {
        json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "bad_request" }))
    }
}

/// The response for a failed journal operation.
fn decision_error(error: &DecisionError) -> Response<BoxBody> {
    match error {
        DecisionError::NotFound => json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": "not_found" })),
        DecisionError::Unavailable => unavailable(),
    }
}

/// The 500 for a journal that cannot be read.
fn unavailable() -> Response<BoxBody> {
    json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        &serde_json::json!({ "error": "journal_unavailable" }),
    )
}

/// Compares two byte strings without leaking where they first differ.
///
/// A token check that returns early on the first wrong byte tells an attacker
/// how many leading bytes were right; comparing all of them does not.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::{Panes, constant_time_eq};
    use crate::decisions::Journal;

    fn panes(token: Option<&str>) -> Panes {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-panes-{}", std::process::id()));
        Panes::new(
            Journal {
                path: dir.join("d.jsonl"),
                outbox: dir.join("outbox"),
            },
            token.map(str::to_string),
        )
    }

    /// The gate is exact and fail-closed: the right token passes, a missing or
    /// wrong one does not, and a kiosk with no token configured lets nobody in.
    #[test]
    fn the_token_gate_fails_closed() {
        let open = panes(Some("s3cret"));
        assert!(open.authorized(Some(&"Bearer s3cret".parse().unwrap())));
        assert!(!open.authorized(Some(&"Bearer wrong".parse().unwrap())));
        assert!(!open.authorized(Some(&"s3cret".parse().unwrap())), "scheme is required");
        assert!(!open.authorized(None));

        let closed = panes(None);
        assert!(!closed.authorized(None));
        assert!(
            !closed.authorized(Some(&"Bearer anything".parse().unwrap())),
            "no token = nobody in"
        );
    }

    /// The comparison is length-checked and content-checked.
    #[test]
    fn constant_time_equality_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
