// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's own panes: the board of decisions, its two rulings, the outbox
//! of reports, the grid that records an intention, and the remotes the Kiosk
//! aggregates.
//!
//! Everything here is behind the token. A kiosk with no token answers 401
//! rather than serving the board — the panes are an operator's view, and the
//! absence of a token is not a licence to read them.

use http::{HeaderValue, Response, StatusCode, header};

use crate::body::{BoxBody, full, json_response};
use crate::decisions::{DecisionError, Journal};
use crate::remotes::{Remote, RemoteError, Remotes};
use crate::reports::{self, IntentError};

/// The panes' state: the journal, the remotes the Kiosk aggregates, and the
/// token that opens them.
#[derive(Debug, Clone)]
pub struct Panes {
    journal: Journal,
    token: Option<String>,
    remotes: Remotes,
}

impl Panes {
    pub const fn new(journal: Journal, token: Option<String>, remotes: Remotes) -> Self {
        Self {
            journal,
            token,
            remotes,
        }
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

    /// `GET /reports` — the outbox, newest first.
    pub fn reports(&self) -> Response<BoxBody> {
        let reports = reports::list(&self.journal.outbox);
        json_response(StatusCode::OK, &serde_json::json!({ "reports": reports }))
    }

    /// `POST /intent` — record an intention as a server-named artefact.
    pub fn intent(&self, content: &str) -> Response<BoxBody> {
        match reports::write_intent(&self.journal.outbox, content) {
            Ok(report) => json_response(
                StatusCode::CREATED,
                &serde_json::json!({ "path": report.path, "name": report.name }),
            ),
            Err(IntentError::Empty) => {
                json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "empty_intent" }))
            }
            Err(IntentError::Unavailable) => unavailable(),
        }
    }

    /// `GET /catalog/list` — the skills catalogue.
    ///
    /// The read-model the page expects: `{ retired, skills }`. Both empty, and
    /// that is the honest state, not a stub for a missing feature — the
    /// catalogue is folded from agents a retire loop distils, and that loop is
    /// not in this tree, so there is nothing to fold yet. Serving the shape
    /// rather than 404 is what lets the tab render its empty state ("aucun
    /// skill") instead of an error the reader cannot act on.
    pub fn catalog() -> Response<BoxBody> {
        json_response(StatusCode::OK, &serde_json::json!({ "retired": [], "skills": [] }))
    }

    /// `GET /kiosk/remotes` — the configured servers, token redacted.
    pub fn remotes_list(&self) -> Response<BoxBody> {
        self.remotes.list().map_or_else(
            |_| unavailable(),
            |remotes| {
                let views: Vec<crate::remotes::RemoteView> = remotes.iter().map(Into::into).collect();
                json_response(StatusCode::OK, &serde_json::json!({ "remotes": views }))
            },
        )
    }

    /// `POST /kiosk/remotes` — add a server. The token is stored on the Kiosk
    /// and never echoed back.
    pub fn remote_add(&self, label: &str, url: &str, token: &str) -> Response<BoxBody> {
        match self.remotes.add(label, url, token) {
            Ok(remote) => {
                let view = crate::remotes::RemoteView::from(&remote);
                json_response(StatusCode::CREATED, &serde_json::json!({ "remote": view }))
            }
            Err(RemoteError::Invalid(why)) => {
                json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": why }))
            }
            Err(error) => Self::remote_error(&error),
        }
    }

    /// `POST /kiosk/remotes/<id>/remove` — forget a server.
    pub fn remote_remove(&self, id: &str) -> Response<BoxBody> {
        match self.remotes.remove(id) {
            Ok(remote) => json_response(StatusCode::OK, &serde_json::json!({ "removed": remote.id })),
            Err(error) => Self::remote_error(&error),
        }
    }

    /// The remote with this id, for a route that reaches out to it.
    ///
    /// # Errors
    ///
    /// [`RemoteError::NotFound`] when no remote carries the id.
    pub fn remote(&self, id: &str) -> Result<Remote, RemoteError> {
        self.remotes.get(id)
    }

    /// The response for a remote lookup that failed.
    pub fn remote_error(error: &RemoteError) -> Response<BoxBody> {
        match error {
            RemoteError::NotFound => {
                json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": "no such remote" }))
            }
            _ => unavailable(),
        }
    }

    /// The 405 for a path the Kiosk serves, reached with the wrong method.
    pub fn method_not_allowed(allow: &'static str) -> Response<BoxBody> {
        Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, allow)
            .header(header::CONTENT_TYPE, "application/json")
            .body(full(serde_json::json!({ "error": "method_not_allowed" }).to_string()))
            .expect("static response builder")
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

    /// The 502 for a remote that could not be reached, naming nothing internal.
    pub fn bad_gateway() -> Response<BoxBody> {
        json_response(
            StatusCode::BAD_GATEWAY,
            &serde_json::json!({ "error": "remote_unreachable" }),
        )
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
    use crate::remotes::Remotes;

    fn panes(token: Option<&str>) -> Panes {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-panes-{}", std::process::id()));
        Panes::new(
            Journal {
                path: dir.join("d.jsonl"),
                outbox: dir.join("outbox"),
            },
            token.map(str::to_string),
            Remotes {
                path: dir.join("remotes.json"),
            },
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
