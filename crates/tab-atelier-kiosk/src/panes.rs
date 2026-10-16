// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's own panes: the board of decisions, its two rulings, the outbox
//! of reports, the grid that records an intention, and the remotes the Kiosk
//! aggregates.
//!
//! Everything here is behind the token. A kiosk with no token answers 401
//! rather than serving the board — the panes are an operator's view, and the
//! absence of a token is not a licence to read them.

use std::path::{Path, PathBuf};

use http::{HeaderValue, Response, StatusCode, header};

use crate::body::{BoxBody, full, json_response};
use crate::decisions::{DecisionError, Journal};
use crate::intent::Intentions;
use crate::remotes::{Remote, RemoteError, Remotes};
use crate::reports;

/// The panes' state: the journal, the intentions, the remotes the Kiosk
/// aggregates, the token that opens them, and the directory the UI is served
/// from.
#[derive(Debug, Clone)]
pub struct Panes {
    journal: Journal,
    token: Option<String>,
    remotes: Remotes,
    intentions: Intentions,
    /// The directory the UI is served from. Not embedded: a rebuilt page is a
    /// copy on disk, not a rebuild of the server.
    web_root: PathBuf,
    /// Where the last agent broadcast started. Each broadcast begins at the
    /// next tab rather than always at the first, so repeated fan-outs do not
    /// hammer whichever tab happens to be first.
    broadcast_cursor: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Panes {
    pub fn new(
        journal: Journal,
        token: Option<String>,
        remotes: Remotes,
        intentions: Intentions,
        web_root: PathBuf,
    ) -> Self {
        Self {
            journal,
            token,
            remotes,
            intentions,
            web_root,
            broadcast_cursor: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The directory the UI is served from.
    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    /// A 404 in the Kiosk's JSON shape.
    pub fn not_found() -> Response<BoxBody> {
        json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": "not_found" }))
    }

    /// The cursor for the next broadcast, advanced past the whole set so the
    /// following one starts at a different tab.
    pub fn next_broadcast_cursor(&self, len: usize) -> usize {
        use std::sync::atomic::Ordering;
        let cursor = self.broadcast_cursor.fetch_add(1, Ordering::Relaxed);
        if len == 0 { cursor } else { cursor % len }
    }

    /// The servers the fleet view shows: the ones the operator configured in
    /// Serveurs, each `(id, label, url, token)`. There is no special "local"
    /// server — to see a machine's tabs, add it as a remote, which is the same
    /// configuration that already exists.
    #[must_use]
    pub fn fleet_targets(&self) -> Vec<Remote> {
        self.remotes.list().unwrap_or_default()
    }

    /// Whether a request carries the pane token — in `Authorization: Bearer`, or
    /// in a `?token=` query.
    ///
    /// The query is accepted because some callers cannot set a header: a plain
    /// `<a href>` opening the terminal viewer in a new tab, and any seasoned
    /// operator who pastes `?token=` into the address bar. A page that only
    /// worked when JavaScript attached a header would be a page you cannot open
    /// by URL, which is how a browser is actually used.
    ///
    /// No token configured → nobody is authorized, so the panes fail closed
    /// instead of standing open.
    pub fn authorized(&self, authorization: Option<&HeaderValue>, query_token: Option<&str>) -> bool {
        // The gate lives in the shared web crate, so the kiosk and the proxy
        // compare tokens the same way — Bearer or ?token=, constant-time,
        // fail-closed when no token is set.
        tab_atelier_web::token_matches(self.token.as_deref(), authorization, query_token)
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

    /// `POST /intent` — record an intention from the grid. The first non-empty
    /// line titles it; the whole note is the pitch.
    pub fn intent(&self, content: &str) -> Response<BoxBody> {
        if content.trim().is_empty() {
            return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "empty_intent" }));
        }
        let title = content
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or("intention", |line| line.trim_start_matches('#').trim());
        self.intent_new(title, "", content)
    }

    /// `GET /intent/list` — the intentions, the ready ones last.
    pub fn intents_list(&self) -> Response<BoxBody> {
        self.intentions.list().map_or_else(
            |_| unavailable(),
            |intents| json_response(StatusCode::OK, &serde_json::json!({ "intents": intents })),
        )
    }

    /// `POST /intent/new` — start an intention.
    pub fn intent_new(&self, title: &str, repo: &str, pitch: &str) -> Response<BoxBody> {
        match self.intentions.create(title, repo, pitch) {
            Ok(summary) => json_response(StatusCode::CREATED, &serde_json::json!({ "intent": summary })),
            Err(why) => json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": why })),
        }
    }

    /// `GET /intent/<slug>` — one intention, whole (front matter and turns).
    pub fn intent_view(&self, slug: &str) -> Response<BoxBody> {
        self.intentions.read(slug).map_or_else(
            |why| json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": why })),
            |body| json_response(StatusCode::OK, &serde_json::json!({ "slug": slug, "body": body })),
        )
    }

    /// `POST /intent/<slug>/promote` — move it to the ready pool.
    pub fn intent_promote(&self, slug: &str) -> Response<BoxBody> {
        self.intentions.promote(slug).map_or_else(
            |why| json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": why })),
            |()| json_response(StatusCode::OK, &serde_json::json!({ "promoted": slug })),
        )
    }

    /// `POST /intent/<slug>/ask` — append a turn from the operator.
    pub fn intent_ask(&self, slug: &str, text: &str) -> Response<BoxBody> {
        self.intentions.append_turn(slug, "Question", text).map_or_else(
            |why| json_response(StatusCode::NOT_FOUND, &serde_json::json!({ "error": why })),
            |body| json_response(StatusCode::OK, &serde_json::json!({ "slug": slug, "body": body })),
        )
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

    /// The intentions store, for the launch route — it reads the intention to
    /// brief the worker and records the tab that came back.
    pub const fn intentions(&self) -> &Intentions {
        &self.intentions
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

#[cfg(test)]
mod tests {
    use super::Panes;
    use crate::decisions::Journal;
    use crate::intent::Intentions;
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
            Intentions::new(dir.join("intentions")).expect("intentions dir"),
            std::path::PathBuf::new(),
        )
    }

    /// The gate is exact and fail-closed: the right token passes, a missing or
    /// wrong one does not, and a kiosk with no token configured lets nobody in.
    #[test]
    fn the_token_gate_fails_closed() {
        let open = panes(Some("s3cret"));
        assert!(open.authorized(Some(&"Bearer s3cret".parse().unwrap()), None));
        assert!(open.authorized(None, Some("s3cret")), "the query token is accepted too");
        assert!(!open.authorized(Some(&"Bearer wrong".parse().unwrap()), None));
        assert!(
            !open.authorized(Some(&"s3cret".parse().unwrap()), None),
            "scheme is required"
        );
        assert!(!open.authorized(None, Some("wrong")), "a wrong query token is refused");
        assert!(!open.authorized(None, None));

        let closed = panes(None);
        assert!(!closed.authorized(None, None));
        assert!(!closed.authorized(None, Some("anything")), "no token = nobody in");
    }
    // The constant-time comparison itself is tested in `tab-atelier-web`, where
    // it now lives.
}
