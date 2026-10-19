// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's request and response resources — the shapes its API speaks.
//!
//! Every route answers with [`Doc`], which carries a typed resource and the
//! status it goes out with. `Doc` also teaches the `OpenAPI` figure what that
//! resource looks like, which is the point: a route whose body is a bare
//! `serde_json::json!(…)` documents as nothing, and the drift test cannot tell
//! a documented route from an undocumented one.
//!
//! The inner values are the domain types the stores already return
//! ([`Decision`], [`Report`], [`Skill`], [`Summary`], [`RemoteView`]) — the same
//! bytes the hand-written `json!` bodies produced, now named so a schema falls
//! out of them.

use bytes::Bytes;
use rocket::http::Status;
use rocket::request::Request;
use rocket::response::{Responder, Result as ResponseResult};
use serde::{Deserialize, Serialize};

use tab_atelier_api::request::{Rejection, Validated};

use tab_atelier_api::marshal::json_of;
use tab_atelier_api::resource::ProblemResource;
use tab_atelier_api::rocket_okapi::r#gen::OpenApiGenerator;
use tab_atelier_api::rocket_okapi::okapi::openapi3::Responses;
use tab_atelier_api::rocket_okapi::response::OpenApiResponderInner;
use tab_atelier_api::rocket_okapi::util;
use tab_atelier_api::schemars::{self, JsonSchema};

use crate::catalog::Skill;
use crate::intent::Summary;
use crate::remotes::RemoteView;
use crate::reports::Report;
use tab_atelier_decisions::Decision;

/// A JSON resource and the status it is answered with.
///
/// The Kiosk's one response shape. It renders through [`json_of`], so the bytes
/// on the wire are what the hand-written bodies produced byte for byte, and it
/// documents through `T`'s schema, so `#[openapi]` can describe the route.
#[derive(Debug, Clone)]
pub struct Doc<T> {
    status: Status,
    value: T,
}

impl<T> Doc<T> {
    /// A resource answered with `200 OK`.
    pub const fn ok(value: T) -> Self {
        Self {
            status: Status::Ok,
            value,
        }
    }

    /// A resource answered with `201 Created`.
    pub const fn created(value: T) -> Self {
        Self {
            status: Status::Created,
            value,
        }
    }

    /// A resource answered with an explicit status.
    pub const fn with_status(status: Status, value: T) -> Self {
        Self { status, value }
    }
}

impl Doc<ProblemResource> {
    /// The refusal shape every error on this API uses.
    ///
    /// A `Doc<ProblemResource>` rather than a bare `Reply`, so a route that
    /// can fail documents both the success and the `{"error": …}` body. It
    /// lives on `Doc<ProblemResource>` and not the generic block so a caller
    /// writes `Doc::problem(…)` — in the generic block the `Self` type would be
    /// unconstrained and would not infer.
    #[must_use]
    pub fn problem(status: u16, message: &str) -> Self {
        Self {
            status: Status::new(status),
            value: ProblemResource {
                error: message.to_owned(),
            },
        }
    }
}

impl<'r, T: Serialize> Responder<'r, 'static> for Doc<T> {
    fn respond_to(self, request: &'r Request<'_>) -> ResponseResult<'static> {
        json_of(self.status.code, &self.value).respond_to(request)
    }
}

impl<T: JsonSchema> OpenApiResponderInner for Doc<T> {
    fn responses(r#gen: &mut OpenApiGenerator) -> tab_atelier_api::rocket_okapi::Result<Responses> {
        let schema = r#gen.json_schema::<T>();
        let mut responses = Responses::default();
        util::add_schema_response(&mut responses, 200, "application/json", schema)?;
        Ok(responses)
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A route's answer: the resource, or the refusal shape.
pub type Answer<T> = Result<Doc<T>, Doc<ProblemResource>>;

/// `GET /decisions` — the board, newest first.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Decisions {
    /// The decisions, newest first.
    pub decisions: Vec<Decision>,
}

/// One decision, the answer to a read or a ruling.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DecisionOne {
    /// The decision.
    pub decision: Decision,
}

/// `GET /reports` — the outbox listing.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Reports {
    /// The reports in the outbox.
    pub reports: Vec<Report>,
}

/// `GET /intent/list` — the intentions, ready ones last.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IntentList {
    /// The intentions.
    pub intents: Vec<Summary>,
}

/// One intention, newly started.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IntentOne {
    /// The intention summary.
    pub intent: Summary,
}

/// An intention's whole body.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IntentBody {
    /// The intention's slug.
    pub slug: String,
    /// The intention's markdown body.
    pub body: String,
}

/// The acknowledgement that an intention was promoted.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IntentPromoted {
    /// The slug that moved to the ready pool.
    pub promoted: String,
}

/// `GET /catalog/list` — the skills catalogue.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CatalogList {
    /// Skills the catalogue has retired.
    pub retired: Vec<Skill>,
    /// Skills the catalogue still holds.
    pub skills: Vec<Skill>,
}

/// The acknowledgement that a skill was edited.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CatalogEdited {
    /// The skill that changed.
    pub edited: String,
    /// The prompt version now in force, when the skill carries one.
    #[serde(rename = "promptVersion")]
    pub prompt_version: Option<u32>,
}

/// The acknowledgement that a skill's visibility changed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CatalogVisibility {
    /// The skill whose visibility changed.
    pub skill: String,
    /// Whether it is now tombstoned.
    pub deleted: bool,
}

/// `GET /kiosk/remotes` — the servers the Kiosk aggregates.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Remotes {
    /// The configured remotes, their tokens never included.
    pub remotes: Vec<RemoteView>,
}

/// A remote, freshly added.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RemoteAdded {
    /// The remote, its token never included.
    pub remote: RemoteView,
}

/// The acknowledgement that a remote was forgotten.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RemoteRemoved {
    /// The id of the remote that was removed.
    pub removed: String,
}

/// `GET /kiosk/fleet` — every server and its tabs, aggregated.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Fleet {
    /// One entry per configured remote.
    pub servers: Vec<FleetServer>,
}

/// One server in the fleet, reachable or not.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FleetServer {
    /// The remote's id.
    pub id: String,
    /// The remote's label.
    pub label: String,
    /// The remote's tabs, each with its token stripped — present when the
    /// remote answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Vec<serde_json::Value>>,
    /// `unreachable` when the remote could not be reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /kiosk/remotes/<id>/cards` — a remote's agent cards.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Cards {
    /// The cards, derived from each tab's meta.
    pub cards: Vec<serde_json::Value>,
}

/// The acknowledgement that a broadcast started.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BroadcastStarted {
    /// How many tabs the message was queued for.
    pub started: usize,
}

/// The acknowledgement that a tab was closed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct TabClosed {
    /// The id of the tab that was closed.
    pub closed: String,
}

/// The result of launching a worker tab for an intention.
#[derive(Debug, Serialize, JsonSchema)]
pub struct TabLaunched {
    /// The intention's slug.
    pub slug: String,
    /// The tab that answered, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// Whether a tab was opened.
    pub launched: bool,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// `POST /decisions/<id>/tranch` — rule a decision.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TranchRequest {
    /// The verdict to record (e.g. `approved`, `rejected`).
    pub verdict: String,
}

/// `POST /intent` — record an intention inline.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IntentRequest {
    /// The intention's raw text.
    pub content: String,
}

/// `POST /intent/new` — start an intention.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NewIntentRequest {
    /// The intention's title.
    pub title: String,
    /// The repository it concerns.
    #[serde(default)]
    pub repo: String,
    /// The pitch the intention argues.
    #[serde(default)]
    pub pitch: String,
}

/// `POST /intent/<slug>/ask` — append a turn to an intention.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskRequest {
    /// The turn's text.
    pub text: String,
}

/// `POST /kiosk/remotes` — add a server.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddRemoteRequest {
    /// The remote's label.
    pub label: String,
    /// The remote's base URL.
    pub url: String,
    /// The remote's token.
    pub token: String,
}

// Each write body parses into its struct shape and leaves the cross-field
// judgement to the store, which already makes it: a blank label, an
// unparseable URL, an unknown ruling — those are the store's refusals and its
// own statuses, not a second opinion duplicated here. So `validate` has nothing
// to add, and `accept` is the shared parse. Written through one macro because
// the five are identical and a divergence should be deliberate, not a typo.

macro_rules! validated_through_parse {
    ($($body:ty),+ $(,)?) => {
        $(
            impl Validated for $body {
                fn validate(&self) -> Result<(), String> {
                    Ok(())
                }

                fn accept(body: &Bytes) -> Result<Self, Rejection> {
                    tab_atelier_api::parse(body)
                }
            }
        )+
    };
}

validated_through_parse!(
    TranchRequest,
    IntentRequest,
    NewIntentRequest,
    AskRequest,
    AddRemoteRequest,
);
