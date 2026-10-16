// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's routes, as Rocket attributes.
//!
//! This replaces the hand-written path classifier: what used to be a `match` on
//! a path string and a `Method` is now a mount table Rocket owns, so the router
//! and the `Allow` header are derived from the routes themselves rather than
//! kept in a second list that drifts from the first.
//!
//! Every JSON route carries `#[openapi]` and answers with a
//! [`Doc`](crate::resources::Doc), so the `OpenAPI` figure the Kiosk serves is
//! derived from these signatures. The three routes that hand back an opaque
//! body — the UI assets, a decision's linked file, and a remote's tab list as
//! the remote sent it — are plain `#[get]`s returning a
//! [`Reply`](tab_atelier_api::marshal::Reply) and are exempted from the figure.

// Rocket hands a handler its guards by value — that is what `FromRequest` and
// `FromData` produce — so a guard like `Actor<KioskAuth>` or a `Json<T>` body
// is owned on arrival even when the handler only reads it. Clippy's advice to
// pass by reference is not something a route signature can take; the whole
// module is route handlers, so it is silenced here rather than on each.
#![allow(clippy::needless_pass_by_value)]

use std::path::PathBuf;

use rocket::State;
use rocket::serde::json::{Json as RawJson, Value};

use rocket::{get, post, routes};
use tab_atelier_api::body::Json;
use tab_atelier_api::guard::Actor;
use tab_atelier_api::marshal::Reply;
use tab_atelier_api::resource::ProblemResource;
use tab_atelier_api::rocket_okapi::openapi;
use tab_atelier_api::rocket_okapi::openapi_get_routes_spec;

use crate::auth::KioskAuth;
use crate::daemon::Daemon;
use crate::panes::Panes;
use crate::resources::{
    AddRemoteRequest, AskRequest, BroadcastStarted, Cards, CatalogEdited, CatalogList, CatalogVisibility, DecisionOne,
    Decisions, Doc, Fleet, IntentBody, IntentList, IntentOne, IntentPromoted, IntentRequest, NewIntentRequest,
    RemoteAdded, RemoteRemoved, Remotes, Reports, TabClosed, TabLaunched, TranchRequest,
};
use crate::server;

/// A JSON route's answer: the resource, or the refusal shape.
type Answer<T> = Result<Doc<T>, Doc<ProblemResource>>;

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// `GET /` — the Kiosk front page.
#[get("/")]
fn index(panes: &State<Panes>) -> Reply {
    server::serve_asset(panes, "kiosk.html")
}

/// `GET /kiosk` — the Kiosk front page, spelled out.
#[get("/kiosk")]
fn kiosk(panes: &State<Panes>) -> Reply {
    server::serve_asset(panes, "kiosk.html")
}

/// `GET /kiosk/manage` — the management page.
#[get("/kiosk/manage")]
fn manage(panes: &State<Panes>) -> Reply {
    server::serve_asset(panes, "manage.html")
}

/// `GET /assets/<rel..>` — a UI file from the web root.
#[get("/assets/<rel..>")]
fn asset(panes: &State<Panes>, rel: PathBuf) -> Reply {
    let rel = rel.to_string_lossy().into_owned();
    server::serve_asset(panes, &rel)
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// `GET /decisions` — the board.
#[openapi(tag = "decisions")]
#[get("/decisions?<includeArchived>")]
#[allow(non_snake_case)]
fn decisions(panes: &State<Panes>, _actor: Actor<KioskAuth>, includeArchived: Option<bool>) -> Answer<Decisions> {
    panes.list(includeArchived.unwrap_or(false))
}

/// `GET /decisions/file?<path>` — a file a decision links.
///
/// The bytes and their content type come from the file itself, so this one is
/// not a JSON resource and is exempt from the `OpenAPI` figure.
#[get("/decisions/file?<path>")]
fn decision_file(panes: &State<Panes>, _actor: Actor<KioskAuth>, path: String) -> Reply {
    panes.file(&path)
}

/// `POST /decisions/<id>/read` — mark a decision read.
#[openapi(tag = "decisions")]
#[post("/decisions/<id>/read")]
fn decision_read(panes: &State<Panes>, _actor: Actor<KioskAuth>, id: &str) -> Answer<DecisionOne> {
    panes.read(id)
}

/// `POST /decisions/<id>/tranch` — rule a decision.
#[openapi(tag = "decisions")]
#[post("/decisions/<id>/tranch", data = "<body>")]
fn decision_tranch(
    panes: &State<Panes>,
    _actor: Actor<KioskAuth>,
    id: &str,
    body: Json<TranchRequest>,
) -> Answer<DecisionOne> {
    panes.tranch(id, &body.verdict)
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// `GET /reports` — the outbox.
#[openapi(tag = "reports")]
#[get("/reports")]
fn reports(panes: &State<Panes>, _actor: Actor<KioskAuth>) -> Doc<Reports> {
    panes.reports()
}

// ---------------------------------------------------------------------------
// Intentions
// ---------------------------------------------------------------------------

/// `POST /intent` — record an intention inline.
#[openapi(tag = "intent")]
#[post("/intent", data = "<body>")]
fn intent(panes: &State<Panes>, _actor: Actor<KioskAuth>, body: Json<IntentRequest>) -> Answer<IntentOne> {
    panes.intent(&body.content)
}

/// `GET /intent/list` — the intentions.
#[openapi(tag = "intent")]
#[get("/intent/list")]
fn intent_list(panes: &State<Panes>, _actor: Actor<KioskAuth>) -> Answer<IntentList> {
    panes.intents_list()
}

/// `POST /intent/new` — start an intention.
#[openapi(tag = "intent")]
#[post("/intent/new", data = "<body>")]
fn intent_new(panes: &State<Panes>, _actor: Actor<KioskAuth>, body: Json<NewIntentRequest>) -> Answer<IntentOne> {
    panes.intent_new(&body.title, &body.repo, &body.pitch)
}

/// `GET /intent/<slug>` — one intention, whole.
#[openapi(tag = "intent")]
#[get("/intent/<slug>")]
fn intent_view(panes: &State<Panes>, _actor: Actor<KioskAuth>, slug: &str) -> Answer<IntentBody> {
    panes.intent_view(slug)
}

/// `POST /intent/<slug>/promote` — move it to the ready pool.
#[openapi(tag = "intent")]
#[post("/intent/<slug>/promote")]
fn intent_promote(panes: &State<Panes>, _actor: Actor<KioskAuth>, slug: &str) -> Answer<IntentPromoted> {
    panes.intent_promote(slug)
}

/// `POST /intent/<slug>/ask` — append a turn.
#[openapi(tag = "intent")]
#[post("/intent/<slug>/ask", data = "<body>")]
fn intent_ask(
    panes: &State<Panes>,
    _actor: Actor<KioskAuth>,
    slug: &str,
    body: Json<AskRequest>,
) -> Answer<IntentBody> {
    panes.intent_ask(slug, &body.text)
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// `GET /catalog/list` — the skills catalogue.
#[openapi(tag = "catalog")]
#[get("/catalog/list?<includeDeleted>")]
#[allow(non_snake_case)]
fn catalog_list(panes: &State<Panes>, _actor: Actor<KioskAuth>, includeDeleted: Option<bool>) -> Answer<CatalogList> {
    panes.catalog(includeDeleted.unwrap_or(false))
}

/// `POST /catalog/<skill>/edit` — write a skill's profile.
#[openapi(tag = "catalog")]
#[post("/catalog/<skill>/edit", data = "<body>")]
fn catalog_edit(
    panes: &State<Panes>,
    _actor: Actor<KioskAuth>,
    skill: &str,
    body: RawJson<Value>,
) -> Answer<CatalogEdited> {
    panes.catalog_edit(skill, &body.0)
}

/// `POST /catalog/<skill>/delete` — tombstone a skill.
#[openapi(tag = "catalog")]
#[post("/catalog/<skill>/delete")]
fn catalog_delete(panes: &State<Panes>, _actor: Actor<KioskAuth>, skill: &str) -> Answer<CatalogVisibility> {
    panes.catalog_visibility(skill, true)
}

/// `POST /catalog/<skill>/restore` — un-tombstone a skill.
#[openapi(tag = "catalog")]
#[post("/catalog/<skill>/restore")]
fn catalog_restore(panes: &State<Panes>, _actor: Actor<KioskAuth>, skill: &str) -> Answer<CatalogVisibility> {
    panes.catalog_visibility(skill, false)
}

// ---------------------------------------------------------------------------
// Remotes
// ---------------------------------------------------------------------------

/// `GET /kiosk/remotes` — the servers the Kiosk aggregates.
#[openapi(tag = "remotes")]
#[get("/kiosk/remotes")]
fn remotes_list(panes: &State<Panes>, _actor: Actor<KioskAuth>) -> Answer<Remotes> {
    panes.remotes_list()
}

/// `POST /kiosk/remotes` — add a server.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes", data = "<body>")]
fn remote_add(panes: &State<Panes>, _actor: Actor<KioskAuth>, body: Json<AddRemoteRequest>) -> Answer<RemoteAdded> {
    panes.remote_add(&body.label, &body.url, &body.token)
}

/// `POST /kiosk/remotes/<id>/remove` — forget a server.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/remove")]
fn remote_remove(panes: &State<Panes>, _actor: Actor<KioskAuth>, id: &str) -> Answer<RemoteRemoved> {
    panes.remote_remove(id)
}

// ---------------------------------------------------------------------------
// Fleet
// ---------------------------------------------------------------------------

/// `GET /kiosk/fleet` — every server and its tabs, aggregated server-side.
#[openapi(tag = "fleet")]
#[get("/kiosk/fleet")]
async fn fleet(panes: &State<Panes>, daemon: &State<Daemon>, _actor: Actor<KioskAuth>) -> Answer<Fleet> {
    server::fleet(panes, daemon).await
}

/// `GET /kiosk/remotes/<id>/cards` — a remote's agent cards.
#[openapi(tag = "fleet")]
#[get("/kiosk/remotes/<id>/cards")]
async fn remote_cards(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
) -> Answer<Cards> {
    server::remote_cards(panes, daemon, id).await
}

// ---------------------------------------------------------------------------
// A remote's tabs — relayed, or acted on
// ---------------------------------------------------------------------------

/// `GET /kiosk/remotes/<id>/tabs` — a remote's tabs, relayed as it sent them.
#[openapi(tag = "remotes")]
#[get("/kiosk/remotes/<id>/tabs")]
async fn remote_tabs(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
) -> Answer<Value> {
    server::remote_tabs(panes, daemon, id).await
}

/// `POST /kiosk/remotes/<id>/tabs` — open a tab on a remote, relaying its answer.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/tabs", data = "<body>")]
async fn remote_tab_create(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
    body: RawJson<Value>,
) -> Answer<Value> {
    server::remote_tab_create(panes, daemon, id, &body.0).await
}

/// `POST /kiosk/remotes/<id>/tabs/<tab>/close` — close a tab on a remote.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/tabs/<tab>/close")]
async fn remote_tab_close(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
    tab: &str,
) -> Answer<TabClosed> {
    server::remote_tab_close(panes, daemon, id, tab).await
}

/// `POST /kiosk/remotes/<id>/tabs/<tab>/meta` — set a tab's card labels,
/// relaying the remote's answer.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/tabs/<tab>/meta", data = "<body>")]
async fn remote_tab_meta(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
    tab: &str,
    body: RawJson<Value>,
) -> Answer<Value> {
    server::remote_tab_meta(panes, daemon, id, tab, &body.0).await
}

/// `GET /kiosk/remotes/<id>/tabs/<tab>/view` — mint a share token, then send
/// the browser to the remote.
///
/// A redirect, not a resource: it carries a `Location` and an empty body, so it
/// is exempt from the `OpenAPI` figure.
#[get("/kiosk/remotes/<id>/tabs/<tab>/view")]
async fn remote_tab_view(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
    tab: &str,
) -> Reply {
    server::remote_tab_view(panes, daemon, id, tab).await
}

/// `POST /kiosk/remotes/<id>/broadcast` — type the same message into every live
/// agent tab of a remote.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/broadcast")]
async fn remote_broadcast(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
) -> Answer<BroadcastStarted> {
    server::remote_broadcast(panes, daemon, id).await
}

/// `POST /kiosk/remotes/<id>/intent/<slug>/launch` — open a worker tab for an
/// intention.
#[openapi(tag = "remotes")]
#[post("/kiosk/remotes/<id>/intent/<slug>/launch")]
async fn remote_intent_launch(
    panes: &State<Panes>,
    daemon: &State<Daemon>,
    _actor: Actor<KioskAuth>,
    id: &str,
    slug: &str,
) -> Answer<TabLaunched> {
    server::remote_intent_launch(panes, daemon, id, slug).await
}

/// Every route the Kiosk mounts, in mount order, and the document derived from
/// the ones that have one.
///
/// The two lists are separate because okapi's macro insists every route it is
/// handed be `#[openapi]` — a route that serves an opaque body cannot be, so the
/// exempt routes are mounted through `routes!` and described in [`exempt`]. The
/// documented ones are exactly the ones carrying `#[openapi]`; `assert_covers`
/// in the test proves the two lists together are the whole surface.
pub fn all() -> (
    Vec<rocket::Route>,
    tab_atelier_api::rocket_okapi::okapi::openapi3::OpenApi,
) {
    let (documented, spec) = openapi_get_routes_spec![
        decisions,
        decision_read,
        decision_tranch,
        reports,
        intent,
        intent_list,
        intent_new,
        intent_view,
        intent_promote,
        intent_ask,
        catalog_list,
        catalog_edit,
        catalog_delete,
        catalog_restore,
        remotes_list,
        remote_add,
        remote_remove,
        fleet,
        remote_cards,
        remote_tabs,
        remote_tab_create,
        remote_tab_close,
        remote_tab_meta,
        remote_broadcast,
        remote_intent_launch,
    ];
    // The exempt routes are mounted the same way; they simply have no
    // `#[openapi]` for the document to carry.
    let opaque = routes![index, kiosk, manage, asset, decision_file, remote_tab_view];
    let mut routes = documented;
    routes.extend(opaque);
    (routes, spec)
}

/// The routes deliberately left out of the document, each with the reason a
/// reader will judge.
///
/// The six that serve an opaque body rather than a resource — the three fixed
/// pages, the asset catch-all, a decision's linked file, and the viewer
/// redirect — plus the document itself, which the mount adds. Everything else
/// carries `#[openapi]`; the point of `assert_covers` is that this list can only
/// shrink, and an entry matching no route fails.
#[must_use]
pub fn exempt() -> Vec<tab_atelier_api::Exempt> {
    use rocket::http::Method;
    vec![
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/",
            reason: "the landing page, an HTML shell",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/kiosk",
            reason: "the Kiosk page, an HTML shell",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/kiosk/manage",
            reason: "the servers page, an HTML shell",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/assets/<rel..>",
            reason: "every UI file from the web root; a directory cannot be enumerated in the document",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/decisions/file",
            reason: "a decision's linked file, relayed as its own bytes",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/kiosk/remotes/<id>/tabs/<tab>/view",
            reason: "a 302 to the remote's viewer after minting a share token",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/openapi.json",
            reason: "the API document itself, added by the mount",
        },
    ]
}
