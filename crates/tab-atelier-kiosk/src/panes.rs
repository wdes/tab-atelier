// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's own panes: the board of decisions, its two rulings, the outbox
//! of reports, the grid that records an intention, and the remotes the Kiosk
//! aggregates.
//!
//! Everything here is behind the token. A kiosk with no token answers 401
//! rather than serving the board — the panes are an operator's view, and the
//! absence of a token is not a licence to read them.
//!
//! The store methods return domain values ([`Decision`], [`Report`], …); the
//! panes wrap each in the resource its route documents, so the bytes on the
//! wire and the schema in the figure come from the same place.

use std::path::{Path, PathBuf};

use tab_atelier_api::marshal::Reply;
use tab_atelier_api::problem;

use crate::intent::Intentions;
use crate::remotes::{Remote, RemoteError, Remotes};
use crate::reports;
use crate::resources::{
    Answer, CatalogEdited, CatalogList, CatalogVisibility, DecisionOne, Decisions, Doc, IntentBody, IntentList,
    IntentOne, IntentPromoted, RemoteAdded, RemoteRemoved, Remotes as RemotesResource, Reports,
};
use tab_atelier_catalog::{Catalog, CatalogError, EditRequest};
use tab_atelier_decisions::{DecisionError, Journal};

/// The panes' state: the journal, the intentions, the remotes the Kiosk
/// aggregates, the token that opens them, and the directory the UI is served
/// from.
#[derive(Debug, Clone)]
pub struct Panes {
    journal: Journal,
    remotes: Remotes,
    intentions: Intentions,
    /// The directory the UI is served from. Not embedded: a rebuilt page is a
    /// copy on disk, not a rebuild of the server.
    web_root: PathBuf,
    /// The skills catalogue — the Kiosk's own store, like the board.
    catalog: Catalog,
    /// Where the last agent broadcast started. Each broadcast begins at the
    /// next tab rather than always at the first, so repeated fan-outs do not
    /// hammer whichever tab happens to be first.
    broadcast_cursor: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Panes {
    pub fn new(
        journal: Journal,
        remotes: Remotes,
        intentions: Intentions,
        web_root: PathBuf,
        catalog: Catalog,
    ) -> Self {
        Self {
            journal,
            remotes,
            intentions,
            web_root,
            catalog,
            broadcast_cursor: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The directory the UI is served from.
    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    /// The Kiosk's own intentions.
    ///
    /// Exposed for the one route that reads an intention before it leaves this
    /// process: launching a worker for it on a remote reads the file here, and
    /// writes the tab id back afterwards.
    #[must_use]
    pub const fn intentions(&self) -> &Intentions {
        &self.intentions
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

    /// `GET /decisions` — the board.
    pub fn list(&self, include_archived: bool) -> Answer<Decisions> {
        self.journal.list(include_archived).map_or_else(
            |_| Err(journal_unavailable()),
            |decisions| Ok(Doc::ok(Decisions { decisions })),
        )
    }

    /// `GET /decisions/file?path=` — one linked file, from inside the outbox.
    ///
    /// Raw bytes with the file's own content type, so not a JSON resource.
    pub fn file(&self, path: &str) -> Reply {
        match self.journal.read_file(path) {
            Ok((bytes, content_type)) => {
                Reply::bytes(200, bytes.into()).with_header("content-type", content_type.to_owned())
            }
            Err(error) => decision_error(&error),
        }
    }

    /// `POST /decisions/<id>/read` — mark read.
    pub fn read(&self, id: &str) -> Answer<DecisionOne> {
        match self.journal.mark_read(id) {
            Ok(decision) => Ok(Doc::ok(DecisionOne { decision })),
            Err(error) => Err(decision_problem(&error)),
        }
    }

    /// `POST /decisions/<id>/tranch` — rule, with a verdict.
    pub fn tranch(&self, id: &str, verdict: &str) -> Answer<DecisionOne> {
        match self.journal.tranch(id, verdict.trim()) {
            Ok(decision) => Ok(Doc::ok(DecisionOne { decision })),
            Err(error) => Err(decision_problem(&error)),
        }
    }

    /// `GET /reports` — the outbox, newest first.
    ///
    /// The one pane method that cannot fail, so it answers a `Doc` rather than
    /// an `Answer`: reading a directory either lists it or comes back empty,
    /// and there is no status to report beyond 200.
    pub fn reports(&self) -> Doc<Reports> {
        let reports = reports::list(&self.journal.outbox);
        Doc::ok(Reports { reports })
    }

    /// `POST /intent` — record an intention from the grid. The first non-empty
    /// line titles it; the whole note is the pitch.
    pub fn intent(&self, content: &str) -> Answer<IntentOne> {
        if content.trim().is_empty() {
            return Err(Doc::problem(400, "empty_intent"));
        }
        let title = content
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or("intention", |line| line.trim_start_matches('#').trim());
        self.intent_new(title, "", content)
    }

    /// `GET /intent/list` — the intentions, the ready ones last.
    pub fn intents_list(&self) -> Answer<IntentList> {
        self.intentions.list().map_or_else(
            |_| Err(journal_unavailable()),
            |intents| Ok(Doc::ok(IntentList { intents })),
        )
    }

    /// `POST /intent/new` — start an intention.
    pub fn intent_new(&self, title: &str, repo: &str, pitch: &str) -> Answer<IntentOne> {
        match self.intentions.create(title, repo, pitch) {
            Ok(intent) => Ok(Doc::created(IntentOne { intent })),
            Err(why) => Err(Doc::problem(400, &why)),
        }
    }

    /// `GET /intent/<slug>` — one intention, whole (front matter and turns).
    pub fn intent_view(&self, slug: &str) -> Answer<IntentBody> {
        self.intentions.read(slug).map_or_else(
            |why| Err(Doc::problem(404, &why)),
            |body| {
                Ok(Doc::ok(IntentBody {
                    slug: slug.to_owned(),
                    body,
                }))
            },
        )
    }

    /// `POST /intent/<slug>/promote` — move it to the ready pool.
    pub fn intent_promote(&self, slug: &str) -> Answer<IntentPromoted> {
        self.intentions.promote(slug).map_or_else(
            |why| Err(Doc::problem(404, &why)),
            |()| {
                Ok(Doc::ok(IntentPromoted {
                    promoted: slug.to_owned(),
                }))
            },
        )
    }

    /// `POST /intent/<slug>/ask` — append a turn from the operator.
    pub fn intent_ask(&self, slug: &str, text: &str) -> Answer<IntentBody> {
        self.intentions.append_turn(slug, "Question", text).map_or_else(
            |why| Err(Doc::problem(404, &why)),
            |body| {
                Ok(Doc::ok(IntentBody {
                    slug: slug.to_owned(),
                    body,
                }))
            },
        )
    }

    /// `GET /catalog/list` — the skills catalogue.
    ///
    /// The read-model the page expects: `{ retired, skills }`. `retired` is
    /// always empty — that was the legacy v1 fold, and this store has no v1
    /// records, so the page's "category" mode simply groups the active skills.
    /// `skills` is what the operator authored; tombstoned skills are hidden
    /// unless `include_deleted` (the page's "afficher les supprimés").
    pub fn catalog(&self, include_deleted: bool) -> Answer<CatalogList> {
        self.catalog.list(include_deleted).map_or_else(
            |_| Err(journal_unavailable()),
            |skills| {
                Ok(Doc::ok(CatalogList {
                    retired: Vec::new(),
                    skills,
                }))
            },
        )
    }

    /// `POST /catalog/<skill>/edit` — write a skill's profile, creating it if it
    /// is new. This is how a skill is **added**; the version guard is a 409.
    pub fn catalog_edit(&self, skill: &str, body: &serde_json::Value) -> Answer<CatalogEdited> {
        let Ok(request) = serde_json::from_value::<EditRequest>(body.clone()) else {
            return Err(Doc::problem(400, "bad_request"));
        };
        match self.catalog.edit(skill, &request) {
            Ok(edited) => Ok(Doc::ok(CatalogEdited {
                edited: edited.skill,
                prompt_version: edited.prompt_version,
            })),
            Err(CatalogError::EmptyProfile) => Err(Doc::problem(409, "le prompt ne peut pas être vide")),
            Err(CatalogError::Conflict) => Err(Doc::problem(409, "promptVersion périmée")),
            Err(_) => Err(journal_unavailable()),
        }
    }

    /// `POST /catalog/<skill>/delete` or `/restore` — the tombstone toggle.
    ///
    /// The body names the verb in its key (`{"delete": <skill>}` or
    /// `{"restore": <skill>}`), so it is an opaque object rather than a shape
    /// with fixed fields.
    pub fn catalog_visibility(&self, skill: &str, deleted: bool) -> Answer<CatalogVisibility> {
        match self.catalog.set_deleted(skill, deleted) {
            Ok(_) => Ok(Doc::ok(CatalogVisibility {
                skill: skill.to_owned(),
                deleted,
            })),
            Err(CatalogError::NotFound) => Err(Doc::problem(404, "no such skill")),
            Err(_) => Err(journal_unavailable()),
        }
    }

    /// `GET /kiosk/remotes` — the configured servers, token redacted.
    pub fn remotes_list(&self) -> Answer<RemotesResource> {
        self.remotes.list().map_or_else(
            |_| Err(journal_unavailable()),
            |remotes| {
                let remotes: Vec<crate::remotes::RemoteView> = remotes.iter().map(Into::into).collect();
                Ok(Doc::ok(RemotesResource { remotes }))
            },
        )
    }

    /// `POST /kiosk/remotes` — add a server. The token is stored on the Kiosk
    /// and never echoed back.
    pub fn remote_add(&self, label: &str, url: &str, token: &str) -> Answer<RemoteAdded> {
        match self.remotes.add(label, url, token) {
            Ok(remote) => Ok(Doc::created(RemoteAdded {
                remote: crate::remotes::RemoteView::from(&remote),
            })),
            Err(RemoteError::Invalid(why)) => Err(Doc::problem(400, &why)),
            Err(error) => Err(remote_problem(&error)),
        }
    }

    /// `POST /kiosk/remotes/<id>/remove` — forget a server.
    pub fn remote_remove(&self, id: &str) -> Answer<RemoteRemoved> {
        match self.remotes.remove(id) {
            Ok(remote) => Ok(Doc::ok(RemoteRemoved { removed: remote.id })),
            Err(error) => Err(remote_problem(&error)),
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

    /// The 502 for a remote that could not be reached, naming nothing internal.
    pub fn bad_gateway() -> Doc<tab_atelier_api::resource::ProblemResource> {
        Doc::problem(502, "remote_unreachable")
    }
}

/// The 404 problem for a data path that is not there.
pub fn not_found() -> Doc<tab_atelier_api::resource::ProblemResource> {
    Doc::problem(404, "not_found")
}

/// The 500 problem for a journal that cannot be read.
pub fn journal_unavailable() -> Doc<tab_atelier_api::resource::ProblemResource> {
    Doc::problem(500, "journal_unavailable")
}

/// The problem for a failed journal operation.
pub fn decision_problem(error: &DecisionError) -> Doc<tab_atelier_api::resource::ProblemResource> {
    match error {
        DecisionError::NotFound => not_found(),
        DecisionError::Unavailable => journal_unavailable(),
    }
}

/// The problem for a failed remote lookup.
pub fn remote_problem(error: &RemoteError) -> Doc<tab_atelier_api::resource::ProblemResource> {
    match error {
        RemoteError::NotFound => Doc::problem(404, "no such remote"),
        _ => journal_unavailable(),
    }
}

/// The reply for a failed journal read, on the raw-bytes route.
fn decision_error(error: &DecisionError) -> Reply {
    match error {
        DecisionError::NotFound => problem(404, "not_found"),
        DecisionError::Unavailable => problem(500, "journal_unavailable"),
    }
}

// The token gate's own tests live with the guard, in `crate::auth`: the gate is
// the `Actor<KioskAuth>` guard now, not a check the panes make themselves.
