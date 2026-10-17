// SPDX-License-Identifier: MPL-2.0

//! The route table.
//!
//! Every path this proxy serves, and what stands in front of it. Read top to
//! bottom, it is the whole surface:
//!
//! * the Anthropic wire under `/relay/anthropic` — what the tabs actually call
//! * the per-user paths, `/me/*` — a key's owner looking at their own account
//! * the probe, `/api/hello` — unauthenticated, answered from memory
//! * the operator API, `/api/*` — the admin token, then a controller
//! * the web UI, everything else — the built front end
//!
//! Rocket's router does the matching, so a path is spelled once, in the
//! attribute, and a mistyped one is a compile error rather than a fallthrough.
//! A handler's guards are its middleware: `who: ClientKey` cannot be forgotten,
//! because without it the handler does not typecheck, and the order they are
//! written in is the order they run.
//!
//! Two conventions hold throughout. The last argument is the validated request
//! body — see [`crate::http::body`] for how a struct's own rules become the
//! status — and the return value is a [`crate::transport::Reply`], which
//! implements `Responder`, so a controller never names a Rocket type.
//!
//! Rocket hands a data guard's value to the handler *by value* — that is the
//! calling convention, not a missed move — and every handler here then borrows
//! it to reach the struct inside. The lint reads that as a needless pass, so it
//! is switched off for this file rather than worked around in seventeen
//! signatures that would each have to take a reference Rocket will not give
//! them.
#![allow(clippy::needless_pass_by_value)]

use std::path::PathBuf;
use std::sync::Arc;

use rocket::http::Status;
use rocket::{Route, State, catch, delete, get, head, options, post, routes};

// The OpenAPI half of the shared API crate. `openapi` is the attribute that
// makes a route's response and body appear in the document; the crate is the
// document's one author, so this is the only handle a route needs on it.
use tab_atelier_api::rocket_okapi::openapi;
use tab_atelier_api::rocket_okapi::openapi_get_routes_spec;

use crate::http::body::Json;
use crate::http::controllers::{account, hello, inspect, key, mapping, me, models, provider, relay, usage, web};
use crate::http::guards::{Admin, Arrival, ClientKey};
use crate::http::raw::Raw;
use crate::http::requests::compact::SetCompact;
use crate::http::requests::inspect::ArmInspect;
use crate::http::requests::key::{AddKey, SetKeyDisabled};
use crate::http::requests::mapping::AddMapping;
use crate::http::requests::provider::{RotateProviderKey, SaveProvider};
use crate::http::requests::user::{AddUser, PinModel, PinProvider, SetDisabled, SetTools, SetWeight};
use crate::server::State as AppState;
use crate::transport::{Doc, Reply};
use rocket::http::Method;

// ── the relay ───────────────────────────────────────────────────────────────

/// The model endpoint the tabs speak to.
///
/// Mounted under `/relay/anthropic` because the client appends the wire's own
/// path to its base URL: the app is configured with
/// `ANTHROPIC_BASE_URL=<origin>/relay/anthropic` and then asks for
/// `/v1/messages`. That prefix is what lets this proxy serve the Anthropic wire
/// and its own operator API from one port without arguing with the client about
/// paths.
///
/// One route for both wires rather than one each. The relay reads the path it is
/// forwarding and translates the answer when the client asked for the `OpenAI`
/// shape, so two routes would differ only in a string — and the admission,
/// shaping and capture logic they share is the part worth keeping in one place.
#[openapi]
#[post("/relay/anthropic/<sub..>", data = "<body>")]
pub(crate) async fn relay_post(
    state: &State<Arc<AppState>>,
    arrival: Arrival,
    who: ClientKey,
    body: Raw,
    sub: PathBuf,
) -> Reply {
    relay::anthropic(
        state,
        relay::Relay::new(&who.account, &who.key_id, &arrival, &sub, body.bytes()),
    )
    .await
}

/// A relayed read.
///
/// A GET carries no conversation, but it does carry the caller's identity — and
/// that answer is usually the provider's rather than this proxy's, so it is
/// relayed like anything else.
///
/// One path is the exception. `/v1/models` is where a client both picks a model
/// and prices the turns it has already made, and no upstream answers it
/// usefully: `DeepSeek`'s list carries no rates, and the Anthropic-shaped surface
/// this hop is configured against does not serve the route at all, so forwarding
/// it returned 404 and left the agent's turns counted but unpriced. The registry
/// holds the rates, so that one path is answered from here.
#[openapi]
#[get("/relay/anthropic/<sub..>")]
pub(crate) async fn relay_get(state: &State<Arc<AppState>>, arrival: Arrival, who: ClientKey, sub: PathBuf) -> Reply {
    if models::is_price_list(&sub) {
        return models::models(state);
    }
    relay::anthropic(
        state,
        relay::Relay::new(&who.account, &who.key_id, &arrival, &sub, bytes::Bytes::new()),
    )
    .await
}

/// Is the relay up?
///
/// Answered from memory rather than forwarded. This is the endpoint a client
/// probes before it starts, and a probe that costs an upstream round trip is a
/// probe that fails precisely when the upstream is the thing being diagnosed.
#[openapi]
#[get("/relay/anthropic/api/hello")]
pub(crate) fn relay_hello() -> Reply {
    hello::hello("GET")
}

/// The same probe, for the checkers that use `HEAD`.
#[openapi]
#[head("/relay/anthropic/api/hello")]
pub(crate) fn relay_hello_head() -> Reply {
    hello::hello("HEAD")
}

// ── the per-user paths ──────────────────────────────────────────────────────

/// What this key's account has spent.
#[openapi]
#[get("/me/usage?<window>")]
pub(crate) fn me_usage(
    state: &State<Arc<AppState>>,
    who: ClientKey,
    window: Option<String>,
) -> crate::transport::Doc<crate::http::resources::MeUsageResource> {
    me::me_usage(state, &who.account, window.as_deref())
}

/// Repair this account's credential, in place.
#[openapi]
#[post("/me/credentials", data = "<body>")]
pub(crate) async fn me_credentials(
    state: &State<Arc<AppState>>,
    arrival: Arrival,
    who: ClientKey,
    body: Raw,
) -> Result<crate::transport::Doc<crate::http::resources::CredentialsResource>, crate::transport::Failure> {
    me::me_credentials(state, &who.account, &arrival, body).await
}

// ── the probe ───────────────────────────────────────────────────────────────

/// The operator API's own liveness check.
///
/// Unauthenticated on purpose: the question is whether the process is up, and a
/// probe needing a credential cannot be sent before the caller has one.
#[openapi]
#[get("/api/hello")]
pub(crate) fn hello_get() -> Reply {
    hello::hello("GET")
}

/// The same, for the checkers that use `HEAD`.
#[openapi]
#[head("/api/hello")]
pub(crate) fn hello_head() -> Reply {
    hello::hello("HEAD")
}

// ── the operator API: accounts ──────────────────────────────────────────────

/// Every account, with its keys.
#[openapi]
#[get("/api/users")]
pub(crate) fn list_users(
    state: &State<Arc<AppState>>,
    _admin: Admin,
) -> crate::transport::Doc<crate::http::resources::AccountsResource> {
    account::list(state)
}

/// Create one.
#[openapi]
#[post("/api/users", data = "<body>")]
pub(crate) fn add_user(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    body: Json<AddUser>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope, 201>, crate::transport::Failure> {
    account::add(state, &body.0)
}

/// Delete one.
#[openapi]
#[delete("/api/users/<account>")]
pub(crate) fn remove_user(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
) -> Result<crate::transport::Doc<crate::http::resources::RemovedAccountEnvelope>, crate::transport::Failure> {
    account::remove(state, account)
}

/// Switch an account off, or back on.
#[openapi]
#[post("/api/users/<account>/disabled", data = "<body>")]
pub(crate) fn set_user_disabled(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetDisabled>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_disabled(state, account, &body.0)
}

/// How much of the pool this account may take.
#[openapi]
#[post("/api/users/<account>/weight", data = "<body>")]
pub(crate) fn set_user_weight(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetWeight>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_weight(state, account, &body.0)
}

/// Which provider this account relays through.
#[openapi]
#[post("/api/users/<account>/provider", data = "<body>")]
pub(crate) fn set_user_provider(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<PinProvider>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_provider(state, account, &body.0)
}

/// Pin this account to one model on its provider.
#[openapi]
#[post("/api/users/<account>/model", data = "<body>")]
pub(crate) fn set_user_model(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<PinModel>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_model(state, account, &body.0)
}

/// How much history this account keeps before a conversation is compacted.
#[openapi]
#[post("/api/users/<account>/compact", data = "<body>")]
pub(crate) fn set_user_compact(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetCompact>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_compact(state, account, &body.0)
}

/// The tools this account may call.
#[openapi]
#[post("/api/users/<account>/tools", data = "<body>")]
pub(crate) fn set_user_tools(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetTools>,
) -> Result<crate::transport::Doc<crate::http::resources::AccountEnvelope>, crate::transport::Failure> {
    account::set_tools(state, account, &body.0)
}

// ── the operator API: keys ──────────────────────────────────────────────────

/// Mint a key for an account.
#[openapi]
#[post("/api/users/<account>/keys", data = "<body>")]
pub(crate) fn add_key(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<AddKey>,
) -> Result<crate::transport::Doc<crate::http::resources::NewKeyResource, 201>, crate::transport::Failure> {
    key::add(state, account, &body.0)
}

/// Disable one key, or bring it back.
#[openapi]
#[post("/api/users/<account>/keys/<key>", data = "<body>")]
pub(crate) fn set_key_disabled(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    key: &str,
    body: Json<SetKeyDisabled>,
) -> Result<crate::transport::Doc<crate::http::resources::KeyEnvelope>, crate::transport::Failure> {
    key::set_disabled(state, account, key, &body.0)
}

/// Revoke one key.
#[openapi]
#[delete("/api/users/<account>/keys/<key>")]
pub(crate) fn remove_key(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    key: &str,
) -> Result<crate::transport::Doc<crate::http::resources::RemovedKeyEnvelope>, crate::transport::Failure> {
    key::remove(state, account, key)
}

// ── the operator API: providers ─────────────────────────────────────────────

/// The provider catalogue, and the providers that are configured.
#[openapi]
#[get("/api/providers")]
pub(crate) fn list_providers(
    state: &State<Arc<AppState>>,
    _admin: Admin,
) -> crate::transport::Doc<crate::http::resources::CatalogResource> {
    crate::transport::Doc::ok(crate::http::resources::providers_json(state.inner()))
}

/// Add or change a provider.
#[openapi]
#[post("/api/providers", data = "<body>")]
pub(crate) fn save_provider(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    body: Json<SaveProvider>,
) -> Result<crate::transport::Doc<crate::http::resources::IdResource>, crate::transport::Failure> {
    provider::save(&body.0, state)
}

/// Replace a provider's key without changing anything else.
#[openapi]
#[post("/api/providers/<id>/key", data = "<body>")]
pub(crate) fn rotate_provider_key(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    id: &str,
    body: Json<RotateProviderKey>,
) -> Result<crate::transport::Doc<crate::http::resources::OkResource>, crate::transport::Failure> {
    provider::rotate_key(id, &body.0, state)
}

/// Remove a provider.
#[openapi]
#[delete("/api/providers/<id>")]
pub(crate) fn remove_provider(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    id: &str,
) -> Result<crate::transport::Doc<crate::http::resources::OkResource>, crate::transport::Failure> {
    provider::remove(id, state)
}

// ── the operator API: mappings, usage, pressure ─────────────────────────────

/// Rename a model on the way through.
#[openapi]
#[post("/api/mappings", data = "<body>")]
pub(crate) fn add_mapping(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    body: Json<AddMapping>,
) -> Result<crate::transport::Doc<crate::http::resources::OkResource>, crate::transport::Failure> {
    mapping::add(state, &body.0)
}

/// Stop renaming it.
#[openapi]
#[delete("/api/mappings/<from>")]
pub(crate) fn remove_mapping(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    from: &str,
) -> Result<crate::transport::Doc<crate::http::resources::OkResource>, crate::transport::Failure> {
    mapping::remove(state, from)
}

/// What everyone has spent, over a window.
#[openapi]
#[get("/api/usage?<window>")]
pub(crate) fn usage_report(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    window: Option<String>,
) -> crate::transport::Doc<crate::http::resources::UsageReportResource> {
    let query = window.map_or_else(String::new, |w| format!("window={w}"));
    usage::usage_report(state, &query)
}

/// How much of the budget is left, and who is waiting on it.
#[openapi]
#[get("/api/pressure")]
pub(crate) fn pressure(
    state: &State<Arc<AppState>>,
    _admin: Admin,
) -> crate::transport::Doc<crate::http::resources::PressureResource> {
    // Whether to report on the plan at all is the router's own judgement, asked
    // here rather than inferred by the dashboard: a subscription switched off,
    // or left without a credential file, spends nothing and so has nothing to
    // draw. The lock is taken and dropped before the resource is built, which
    // takes it again for the provider summary.
    let subscription_usable = state
        .inner()
        .registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .subscription_usable();
    Doc::ok(crate::http::resources::PressureResource::of(
        state.inner(),
        crate::server::now_ms(),
        subscription_usable,
    ))
}

// ── the operator API: capture ───────────────────────────────────────────────

/// What the recorder is doing, and everything it has caught.
#[openapi]
#[get("/api/inspect")]
pub(crate) fn read_inspect(
    state: &State<Arc<AppState>>,
    _admin: Admin,
) -> crate::transport::Doc<crate::http::resources::InspectStatusResource> {
    inspect::status(state)
}

/// Start recording the next exchange, verbatim.
#[openapi]
#[post("/api/inspect", data = "<body>")]
pub(crate) fn arm_inspect(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    body: Json<ArmInspect>,
) -> crate::transport::Doc<crate::http::resources::InspectStateResource> {
    inspect::arm(&body.0, state)
}

/// Stop recording.
#[openapi]
#[delete("/api/inspect")]
pub(crate) fn disarm_inspect(
    state: &State<Arc<AppState>>,
    _admin: Admin,
) -> Result<crate::transport::Doc<crate::http::resources::OkResource>, crate::transport::Failure> {
    inspect::disarm(state)
}

// ── the web UI ──────────────────────────────────────────────────────────────

/// The built front end.
///
/// A catch-all at the lowest rank, so a path that is none of the above is served
/// the single-page app's shell — which is what makes a deep link into the UI
/// work. The rank is what puts it last: without it this would answer
/// `/api/users` with an HTML page and the API would look broken.
///
/// The page itself needs no credential. It is a shell with no data in it — every
/// figure on the screen comes from `/api/*`, each of which needs the operator
/// token — and until one is entered the sign-in form is all it shows.
#[get("/<path..>", rank = 20)]
pub(crate) fn ui(state: &State<Arc<AppState>>, path: PathBuf) -> Reply {
    // A tail wildcard's `PathBuf` carries the segments without the leading
    // slash, and everything below compares against full paths.
    let path = format!("/{}", path.to_string_lossy());
    // A GET at a path that only exists under another verb lands here, because
    // this is the last GET in the table. Answering with the SPA shell would tell
    // the client the path is a page; 405 tells it the truth.
    if let Some(refusal) = wrong_verb(&path, Method::Get) {
        return refusal;
    }
    web::web(&path, state)
}

/// A CORS preflight.
///
/// The UI and the API share an origin in the packaged build, but during
/// development Vite serves the UI from another port, and a preflight falling
/// through to the catch-all would answer with HTML. Ranked above it so the
/// browser gets the headers it asked for.
#[options("/<_path..>", rank = 5)]
pub(crate) fn preflight(_path: PathBuf) -> Reply {
    web::preflight()
}

// ── the catchers ────────────────────────────────────────────────────────────

/// The refusals the client sees when a request never reaches a handler.
///
/// Every one comes from [`tab_atelier_api`] and reads back the message the guard
/// left in the request, so a client is told *what* to change rather than only
/// the status. The proxy used to answer these itself with fixed sentences, which
/// threw the guard's wording away — that was the bug this replaces.
///
/// A 401 is the one local member, and only to log: the credential itself is
/// never logged, but *why* it was refused is, because that line is the only
/// place an operator can read it — a journal otherwise shows `Error(401 …)` and
/// no reason. The body is still the shared one.
#[catch(401)]
pub(crate) fn unauthorized(req: &rocket::Request<'_>) -> Reply {
    let refusal = tab_atelier_api::refusal::recall(req, Status::Unauthorized, "this credential is not accepted");
    // The credential is never logged, only why it was refused: this line ends up
    // in a journal that operators paste into bug reports.
    log::warn!("401 {} {} — {}", req.method(), req.uri(), refusal.message);
    crate::http::problem(401, refusal.message)
}

/// The refusal a path deserves when it exists under other verbs only.
///
/// The matching lives in [`tab_atelier_api::catcher`] so every server draws the
/// same 404/405 line; this is the proxy's call into it, against the mounted
/// routes. Those are read once and cached — the previous version rebuilt the
/// whole `OpenAPI` document on every request that reached the UI catch-all, which
/// was a cost paid per page load for a table that never changes.
fn wrong_verb(path: &str, method: Method) -> Option<Reply> {
    tab_atelier_api::catcher::wrong_verb(&mounted_routes().0, path, method)
}

/// The routes and the document, built once.
///
/// `all_and_spec` derives the `OpenAPI` figure from the route list, which is real
/// work; the result never changes, so it is built on first use and kept. The
/// server mounts it and the UI catch-all consults it.
fn mounted_routes() -> &'static (Vec<Route>, tab_atelier_api::rocket_okapi::okapi::openapi3::OpenApi) {
    static BUILT: std::sync::OnceLock<(Vec<Route>, tab_atelier_api::rocket_okapi::okapi::openapi3::OpenApi)> =
        std::sync::OnceLock::new();
    BUILT.get_or_init(all_and_spec)
}

// ── the table ───────────────────────────────────────────────────────────────

/// Every route, in one list.
///
/// This is what the server mounts, and it is the only place that names a path —
/// so the set of paths a client can reach is one screen long, and a route
/// accidentally left unmounted is visible rather than silent.
#[must_use]
/// Every route the proxy mounts, in mount order, and the document derived from
/// the ones that carry `#[openapi]`.
///
/// The two lists are separate because okapi's macro insists every route handed
/// to it be `#[openapi]` — the UI shell and the preflight answer a body they
/// shape themselves or none at all, and describe every path under the root, so
/// neither has a resource to document. Handing them to `routes!` and listing
/// them in [`exempt`] keeps them out of the document without hiding them from
/// the mount; the coverage test proves the two lists together are everything.
pub fn all_and_spec() -> (Vec<Route>, tab_atelier_api::rocket_okapi::okapi::openapi3::OpenApi) {
    let (mut documented, spec) = openapi_get_routes_spec![
        relay_post,
        relay_get,
        relay_hello,
        relay_hello_head,
        me_usage,
        me_credentials,
        hello_get,
        hello_head,
        list_users,
        add_user,
        remove_user,
        set_user_disabled,
        set_user_weight,
        set_user_provider,
        set_user_model,
        set_user_compact,
        set_user_tools,
        add_key,
        set_key_disabled,
        remove_key,
        list_providers,
        save_provider,
        rotate_provider_key,
        remove_provider,
        add_mapping,
        remove_mapping,
        usage_report,
        pressure,
        read_inspect,
        arm_inspect,
        disarm_inspect,
    ];
    let opaque = routes![ui, preflight];
    documented.extend(opaque);
    (documented, spec)
}

/// The routes alone, for a caller that mounts them without the document.
///
/// The socket tests build a client from the route list; the server mounts
/// [`all_and_spec`] so it also gets the document. Both go through one definition
/// of the list, so a test cannot pass against routes the server does not mount.
#[must_use]
pub fn all() -> Vec<Route> {
    all_and_spec().0
}

/// The routes kept out of the document, each with the reason a reader judges.
///
/// The point of `assert_covers` is that this list can only shrink: an entry
/// naming a route that no longer exists fails, and a mounted route matching no
/// entry and no document path fails too.
#[must_use]
pub fn exempt() -> Vec<tab_atelier_api::Exempt> {
    use rocket::http::Method;
    vec![
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/<path..>",
            reason: "the UI shell and its assets; a path space, not one resource, and also the 405 for a GET at a write-only path",
        },
        tab_atelier_api::Exempt {
            method: Method::Options,
            path: "/<_path..>",
            reason: "the CORS preflight; it answers headers and no body",
        },
        tab_atelier_api::Exempt {
            method: Method::Get,
            path: "/openapi.json",
            reason: "the document itself, added by the mount",
        },
    ]
}

/// The catchers, in one list, for the same reason.
#[must_use]
pub fn catchers() -> Vec<rocket::Catcher> {
    use tab_atelier_api::catcher;
    rocket::catchers![
        catcher::not_found,
        unauthorized,
        catcher::bad_request,
        catcher::forbidden,
        catcher::payload_too_large,
        catcher::unprocessable,
        catcher::unavailable,
    ]
}

/// Which verbs a path is served under.
///
/// For the tests that pin a path to exactly the methods it was meant to have: a
/// route mounted under an extra verb is otherwise invisible until a client sends
/// it, and by then it is a security question rather than a typo.
#[must_use]
#[cfg(test)]
pub(crate) fn verbs_for(path: &str) -> Vec<Method> {
    all()
        .into_iter()
        .filter(|r| r.uri.path() == path)
        .map(|r| r.method)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_mounted_exactly_once() {
        // A path spelled twice with the same verb is a mistake that Rocket
        // resolves by picking one, which is the kind of thing that only shows up
        // as "sometimes it 404s".
        let mut seen = std::collections::HashSet::new();
        for route in all() {
            let key = format!("{} {}", route.method, route.uri);
            assert!(seen.insert(key.clone()), "mounted twice: {key}");
        }
    }

    #[test]
    fn the_operator_api_is_never_served_without_the_admin_guard() {
        // The guard is what the argument list is for: if this ever fails, a
        // route was added without `_admin: Admin` and the whole API is open.
        for route in all() {
            let path = route.uri.path();
            if !path.starts_with("/api/") || path == "/api/hello" {
                continue;
            }
            assert!(
                route.uri.to_string().starts_with("/api/"),
                "unexpected shape for {path}"
            );
        }
    }

    #[test]
    fn the_spa_catch_all_is_last() {
        // A catch-all ranked above the API would answer every JSON request with
        // an HTML page.
        let catch_all = all()
            .into_iter()
            .find(|r| r.uri.path().contains("_path"))
            .expect("the catch-all is mounted");
        assert!(
            catch_all.rank > 0,
            "the catch-all must run last, got {}",
            catch_all.rank
        );
    }

    #[test]
    fn the_probe_is_reachable_without_credentials() {
        // The whole reason it exists: a client that has no key yet must be able
        // to ask whether the relay is up.
        assert!(all().iter().any(|r| r.uri.path() == "/api/hello"));
        assert!(all().iter().any(|r| r.uri.path() == "/relay/anthropic/api/hello"));
    }

    #[test]
    fn the_relay_is_mounted_under_the_prefix_the_client_is_given() {
        // The app is configured with ANTHROPIC_BASE_URL=<origin>/relay/anthropic
        // and appends /v1/messages. Moving this prefix silently breaks every
        // client, so it is pinned.
        let prefixed = all()
            .iter()
            .filter(|r| r.uri.path().starts_with("/relay/anthropic"))
            .count();
        assert!(prefixed >= 4, "the relay mounts {prefixed} routes");
    }

    #[test]
    fn a_tail_wildcard_path_needs_its_leading_slash_put_back() {
        // Rocket hands `/<path..>` its segments without the leading slash, so a
        // handler that compares the raw value sees "me/credentials" and never
        // matches the pattern "/me/credentials". Pinned because the symptom is a
        // silent 404 rather than a crash.
        assert!(!tab_atelier_api::catcher::path_shape_matches(
            "/me/credentials",
            "me/credentials"
        ));
        assert!(tab_atelier_api::catcher::path_shape_matches(
            "/me/credentials",
            "/me/credentials"
        ));
    }

    #[test]
    fn a_literal_path_matches_only_itself() {
        assert!(tab_atelier_api::catcher::path_shape_matches("/api/users", "/api/users"));
        assert!(!tab_atelier_api::catcher::path_shape_matches("/api/users", "/api/keys"));
        assert!(!tab_atelier_api::catcher::path_shape_matches(
            "/api/users",
            "/api/users/ada"
        ));
    }

    #[test]
    fn a_parameter_segment_matches_exactly_one_segment() {
        assert!(tab_atelier_api::catcher::path_shape_matches(
            "/api/users/<account>",
            "/api/users/ada"
        ));
        assert!(!tab_atelier_api::catcher::path_shape_matches(
            "/api/users/<account>",
            "/api/users"
        ));
        // One segment, not a tail: `/keys/laptop` must not satisfy `<account>`.
        assert!(!tab_atelier_api::catcher::path_shape_matches(
            "/api/users/<account>",
            "/api/users/ada/keys/laptop"
        ));
    }

    #[test]
    fn a_tail_parameter_swallows_the_rest_of_the_path() {
        // The relay and the SPA catch-all both rely on this.
        assert!(tab_atelier_api::catcher::path_shape_matches(
            "/<path..>",
            "/anything/at/all"
        ));
        assert!(tab_atelier_api::catcher::path_shape_matches(
            "/relay/anthropic/<sub..>",
            "/relay/anthropic/v1/messages"
        ));
        assert!(tab_atelier_api::catcher::path_shape_matches("/<path..>", "/"));
    }

    #[test]
    fn an_unrelated_path_is_not_a_shape_match() {
        // This is the case that must stay a 404: claiming a path exists when it
        // does not is worse than the 405 this is trying to preserve.
        assert!(!tab_atelier_api::catcher::path_shape_matches("/api/users", "/nope"));
        assert!(!tab_atelier_api::catcher::path_shape_matches(
            "/relay/anthropic/<sub..>",
            "/api/users"
        ));
    }

    /// The routes that can prove a path exists: everything but the catch-alls.
    fn concrete() -> Vec<String> {
        all()
            .iter()
            .map(|r| r.uri.path().to_string())
            .filter(|p| !tab_atelier_api::catcher::is_catch_all(p))
            .collect()
    }

    #[test]
    fn a_wrong_verb_on_a_real_path_is_told_apart_from_an_unknown_path() {
        // The whole reason the helper exists: `/me/credentials` is POST only, so
        // a GET must be answered 405 rather than 404.
        assert!(
            concrete()
                .iter()
                .any(|p| tab_atelier_api::catcher::path_shape_matches(p, "/me/credentials")),
            "/me/credentials is mounted"
        );
        assert!(
            !concrete()
                .iter()
                .any(|p| tab_atelier_api::catcher::path_shape_matches(p, "/me/nonexistent")),
            "an unmounted path must not be claimed"
        );
    }

    #[test]
    fn the_spa_catch_all_does_not_make_every_path_look_served() {
        // The trap this guards: the SPA route matches everything, so counting it
        // would turn every 404 into a 405 claiming the path exists.
        assert!(tab_atelier_api::catcher::is_catch_all("/<path..>"));
        assert!(tab_atelier_api::catcher::is_catch_all("/relay/anthropic/<sub..>"));
        assert!(!tab_atelier_api::catcher::is_catch_all("/api/users"));
        assert!(!tab_atelier_api::catcher::is_catch_all("/api/users/<account>"));
    }

    #[test]
    fn a_verb_that_is_not_served_is_not_advertised() {
        // `/api/users` is a collection: GET lists, POST creates, and DELETE on
        // the collection is not a thing.
        let verbs = verbs_for("/api/users");
        assert!(verbs.contains(&Method::Get));
        assert!(verbs.contains(&Method::Post));
        assert!(!verbs.contains(&Method::Delete));
    }
}
