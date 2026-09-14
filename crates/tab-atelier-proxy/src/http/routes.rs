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
use rocket::{Route, State, catch, delete, get, head, options, post, put, routes};

use crate::http::body::Json;
use crate::http::controllers::{account, hello, inspect, key, mapping, me, provider, relay, usage, web};
use crate::http::guards::{Admin, Arrival, ClientKey, WebAuth};
use crate::http::raw::Raw;
use crate::http::requests::compact::SetCompact;
use crate::http::requests::inspect::ArmInspect;
use crate::http::requests::key::{AddKey, SetKeyDisabled};
use crate::http::requests::mapping::AddMapping;
use crate::http::requests::provider::{RotateProviderKey, SaveProvider};
use crate::http::requests::user::{AddUser, PinModel, PinProvider, SetDisabled, SetTools, SetWeight};
use crate::server::State as AppState;
use crate::transport::Reply;
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
#[post("/relay/anthropic/<sub..>", data = "<body>")]
pub(crate) async fn relay_post(
    state: &State<Arc<AppState>>,
    arrival: Arrival,
    who: ClientKey,
    body: Raw,
    sub: PathBuf,
) -> Reply {
    relay::anthropic(state, relay::Relay::new(&who.0, &arrival, &sub, body.bytes())).await
}

/// A relayed read.
///
/// A GET carries no conversation, but it does carry the caller's identity —
/// Claude Code asks which models exist — and that answer comes from the provider
/// rather than from this proxy, so it is relayed like anything else.
#[get("/relay/anthropic/<sub..>")]
pub(crate) async fn relay_get(state: &State<Arc<AppState>>, arrival: Arrival, who: ClientKey, sub: PathBuf) -> Reply {
    relay::anthropic(state, relay::Relay::new(&who.0, &arrival, &sub, bytes::Bytes::new())).await
}

/// Is the relay up?
///
/// Answered from memory rather than forwarded. This is the endpoint a client
/// probes before it starts, and a probe that costs an upstream round trip is a
/// probe that fails precisely when the upstream is the thing being diagnosed.
#[get("/relay/anthropic/api/hello")]
pub(crate) fn relay_hello() -> Reply {
    hello::hello("GET")
}

/// The same probe, for the checkers that use `HEAD`.
#[head("/relay/anthropic/api/hello")]
pub(crate) fn relay_hello_head() -> Reply {
    hello::hello("HEAD")
}

// ── the per-user paths ──────────────────────────────────────────────────────

/// What this key's account has spent.
#[get("/me/usage?<window>")]
pub(crate) fn me_usage(state: &State<Arc<AppState>>, who: ClientKey, window: Option<String>) -> Reply {
    me::me_usage(state, &who.0, window.as_deref())
}

/// Repair this account's credential, in place.
#[post("/me/credentials", data = "<body>")]
pub(crate) async fn me_credentials(state: &State<Arc<AppState>>, arrival: Arrival, who: ClientKey, body: Raw) -> Reply {
    me::me_credentials(state, &who.0, &arrival, body).await
}

// ── the probe ───────────────────────────────────────────────────────────────

/// The operator API's own liveness check.
///
/// Unauthenticated on purpose: the question is whether the process is up, and a
/// probe needing a credential cannot be sent before the caller has one.
#[get("/api/hello")]
pub(crate) fn hello_get() -> Reply {
    hello::hello("GET")
}

/// The same, for the checkers that use `HEAD`.
#[head("/api/hello")]
pub(crate) fn hello_head() -> Reply {
    hello::hello("HEAD")
}

// ── the operator API: accounts ──────────────────────────────────────────────

/// Every account, with its keys.
#[get("/api/users")]
pub(crate) fn list_users(state: &State<Arc<AppState>>, _admin: Admin) -> Reply {
    account::list(state)
}

/// Create one.
#[post("/api/users", data = "<body>")]
pub(crate) fn add_user(state: &State<Arc<AppState>>, _admin: Admin, body: Json<AddUser>) -> Reply {
    account::add(state, &body.0)
}

/// Delete one.
#[delete("/api/users/<account>")]
pub(crate) fn remove_user(state: &State<Arc<AppState>>, _admin: Admin, account: &str) -> Reply {
    account::remove(state, account)
}

/// Switch an account off, or back on.
#[put("/api/users/<account>/disabled", data = "<body>")]
pub(crate) fn set_user_disabled(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetDisabled>,
) -> Reply {
    account::set_disabled(state, account, &body.0)
}

/// How much of the pool this account may take.
#[put("/api/users/<account>/weight", data = "<body>")]
pub(crate) fn set_user_weight(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetWeight>,
) -> Reply {
    account::set_weight(state, account, &body.0)
}

/// Which provider this account relays through.
#[put("/api/users/<account>/provider", data = "<body>")]
pub(crate) fn set_user_provider(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<PinProvider>,
) -> Reply {
    account::set_provider(state, account, &body.0)
}

/// Pin this account to one model on its provider.
#[put("/api/users/<account>/model", data = "<body>")]
pub(crate) fn set_user_model(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<PinModel>,
) -> Reply {
    account::set_model(state, account, &body.0)
}

/// How much history this account keeps before a conversation is compacted.
#[put("/api/users/<account>/compact", data = "<body>")]
pub(crate) fn set_user_compact(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetCompact>,
) -> Reply {
    account::set_compact(state, account, &body.0)
}

/// The tools this account may call.
#[put("/api/users/<account>/tools", data = "<body>")]
pub(crate) fn set_user_tools(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    body: Json<SetTools>,
) -> Reply {
    account::set_tools(state, account, &body.0)
}

// ── the operator API: keys ──────────────────────────────────────────────────

/// Mint a key for an account.
#[post("/api/users/<account>/keys", data = "<body>")]
pub(crate) fn add_key(state: &State<Arc<AppState>>, _admin: Admin, account: &str, body: Json<AddKey>) -> Reply {
    key::add(state, account, &body.0)
}

/// Disable one key, or bring it back.
#[put("/api/users/<account>/keys/<key>", data = "<body>")]
pub(crate) fn set_key_disabled(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    account: &str,
    key: &str,
    body: Json<SetKeyDisabled>,
) -> Reply {
    key::set_disabled(state, account, key, &body.0)
}

/// Revoke one key.
#[delete("/api/users/<account>/keys/<key>")]
pub(crate) fn remove_key(state: &State<Arc<AppState>>, _admin: Admin, account: &str, key: &str) -> Reply {
    key::remove(state, account, key)
}

// ── the operator API: providers ─────────────────────────────────────────────

/// The provider catalogue, and the providers that are configured.
#[get("/api/providers")]
pub(crate) fn list_providers(state: &State<Arc<AppState>>, _admin: Admin) -> Reply {
    crate::transport::json_of(200, &crate::http::resources::providers_json(state.inner()))
}

/// Add or change a provider.
#[post("/api/providers", data = "<body>")]
pub(crate) fn save_provider(state: &State<Arc<AppState>>, _admin: Admin, body: Json<SaveProvider>) -> Reply {
    provider::save(&body.0, state)
}

/// Replace a provider's key without changing anything else.
#[put("/api/providers/<id>/key", data = "<body>")]
pub(crate) fn rotate_provider_key(
    state: &State<Arc<AppState>>,
    _admin: Admin,
    id: &str,
    body: Json<RotateProviderKey>,
) -> Reply {
    provider::rotate_key(id, &body.0, state)
}

/// Remove a provider.
#[delete("/api/providers/<id>")]
pub(crate) fn remove_provider(state: &State<Arc<AppState>>, _admin: Admin, id: &str) -> Reply {
    provider::remove(id, state)
}

// ── the operator API: mappings, usage, pressure ─────────────────────────────

/// Rename a model on the way through.
#[post("/api/mappings", data = "<body>")]
pub(crate) fn add_mapping(state: &State<Arc<AppState>>, _admin: Admin, body: Json<AddMapping>) -> Reply {
    mapping::add(state, &body.0)
}

/// Stop renaming it.
#[delete("/api/mappings/<from>")]
pub(crate) fn remove_mapping(state: &State<Arc<AppState>>, _admin: Admin, from: &str) -> Reply {
    mapping::remove(state, from)
}

/// What everyone has spent, over a window.
#[get("/api/usage?<window>")]
pub(crate) fn usage_report(state: &State<Arc<AppState>>, _admin: Admin, window: Option<String>) -> Reply {
    let query = window.map_or_else(String::new, |w| format!("window={w}"));
    usage::usage_report(state, &query)
}

/// How much of the budget is left, and who is waiting on it.
#[get("/api/pressure")]
pub(crate) fn pressure(state: &State<Arc<AppState>>, _admin: Admin) -> Reply {
    crate::transport::json_of(
        200,
        &crate::http::resources::PressureResource::of(state.inner(), crate::server::now_ms()),
    )
}

// ── the operator API: capture ───────────────────────────────────────────────

/// What the recorder is doing, and everything it has caught.
#[get("/api/inspect")]
pub(crate) fn read_inspect(state: &State<Arc<AppState>>, _admin: Admin) -> Reply {
    inspect::status(state)
}

/// Start recording the next exchange, verbatim.
#[post("/api/inspect", data = "<body>")]
pub(crate) fn arm_inspect(state: &State<Arc<AppState>>, _admin: Admin, body: Json<ArmInspect>) -> Reply {
    inspect::arm(&body.0, state)
}

/// Stop recording.
#[delete("/api/inspect")]
pub(crate) fn disarm_inspect(state: &State<Arc<AppState>>, _admin: Admin) -> Reply {
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
/// `_auth: WebAuth` is the whole of the gating: the page, its scripts, its
/// styles and the vendored libraries all come through here, so requiring a
/// credential on this route requires one for every one of them. A crawler never
/// gets past the first request.
#[get("/<path..>", rank = 20)]
pub(crate) fn ui(state: &State<Arc<AppState>>, _auth: WebAuth, path: PathBuf) -> Reply {
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

/// The path was not served, or not under that verb.
///
/// Rocket answers a wrong method with a 404 — its router matches on the method
/// first, so it cannot tell "no such path" from "that path, another verb", and
/// its own matcher is not public. This API promised 405 for the second case
/// before, and a client probing a path deserves to be told the path exists, so
/// the distinction is made here against this file's own table.
///
/// Erring towards 404 when unsure is deliberate: a spurious 405 would claim a
/// path exists, which is a worse lie than the one this is fixing.
#[catch(404)]
pub(crate) fn not_found(req: &rocket::Request<'_>) -> Reply {
    wrong_verb(req.uri().path().as_str(), req.method()).unwrap_or_else(|| crate::http::problem(404, "no such route"))
}

/// The refusal a path deserves when it exists under other verbs only.
///
/// `None` means the path is genuinely unknown, and the caller decides what that
/// means — the SPA serves its shell, an API request gets a 404.
///
/// Rocket cannot make this distinction itself: its router matches the method
/// first, so a wrong verb looks exactly like a missing path, and its own matcher
/// is not public. This API promised 405 for the second case before, and a client
/// probing a path deserves to be told the path exists rather than that it does
/// not.
///
/// Catch-alls are excluded on purpose. The SPA route matches every path, so
/// counting it would make every unknown path answer 405 — claiming a whole
/// namespace exists, which is a worse lie than the 404 this is fixing.
fn wrong_verb(path: &str, method: Method) -> Option<Reply> {
    let elsewhere = all().iter().any(|r| {
        let pattern = r.uri.path().to_string();
        r.method != method && !is_catch_all(&pattern) && path_shape_matches(&pattern, path)
    });
    elsewhere.then(|| {
        crate::http::problem(
            405,
            format!("a {method} is not served here; see the OpenAPI document for the verbs"),
        )
    })
}

/// Whether a route pattern is a catch-all — one that matches every path.
///
/// Recognised by its tail wildcard, which is the only form this table uses for
/// one.
#[must_use]
fn is_catch_all(pattern: &str) -> bool {
    pattern
        .split('/')
        .any(|seg| seg.starts_with('<') && seg.ends_with("..>"))
}

/// Whether a route pattern covers a concrete path, ignoring the verb.
///
/// A segment starting with `<` is a parameter, and one ending in `..>` swallows
/// the rest of the path. Both forms are the ones this table uses; anything else
/// is compared literally.
#[must_use]
fn path_shape_matches(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.split('/');
    let mut path = path.split('/');
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            // `<name..>` takes every remaining segment, including none.
            (Some(seg), Some(_)) if seg.starts_with('<') && seg.ends_with("..>") => return true,
            (Some(seg), Some(_)) if seg.starts_with('<') && seg.ends_with('>') => {}
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// A body that arrived malformed or too large.
#[catch(422)]
pub(crate) fn unprocessable() -> Reply {
    crate::http::problem(422, "the request body could not be read")
}

/// A guard that refused: bad key, bad token, no token configured.
///
/// One catcher for all of them, because a guard picks its own status and the
/// reason it left behind is more specific than the status alone. Without this
/// Rocket would answer with its own page and the sentence explaining the actual
/// problem would never reach the operator.
#[catch(400)]
pub(crate) fn bad_request() -> Reply {
    crate::http::problem(400, "the request could not be read")
}

/// Nothing signed in, or the wrong key.
///
/// One catcher for both because a guard picks its own status and leaves its own
/// sentence behind; what it cannot do is add a header to the response, and a
/// browser only prompts when it is challenged. So the challenge goes on here,
/// and only for a gated path — a relay refusal must NOT carry it, because the
/// client is a CLI that would try to interpret it as a browser would.
#[catch(401)]
pub(crate) fn unauthorized(req: &rocket::Request<'_>) -> Reply {
    let refusal = crate::http::refusal::recall(req, Status::Unauthorized, "no valid credential");
    let mut reply = crate::http::problem(401, refusal.message);
    if !crate::http::auth::gated(req.uri().path().as_str(), req.method()) {
        return reply;
    }
    // A missing state means the server was built without it, which some tests
    // do; there is then no secret to mint a nonce with, so the 401 goes out
    // without a challenge rather than panicking.
    if let Some(state) = req.rocket().state::<Arc<AppState>>() {
        reply = reply.with_header(
            "www-authenticate",
            crate::http::auth::challenge(&state.web_auth, crate::usage::now_secs()),
        );
    }
    reply
}

/// A request the account is not allowed to make.
#[catch(403)]
pub(crate) fn forbidden() -> Reply {
    crate::http::problem(403, "this account may not do that")
}

/// A body over the limit.
#[catch(413)]
pub(crate) fn too_large() -> Reply {
    crate::http::problem(413, "the request body is larger than the limit")
}

/// No provider available to take the request.
#[catch(503)]
pub(crate) fn unavailable() -> Reply {
    crate::http::problem(503, "no provider is available right now")
}

// ── the table ───────────────────────────────────────────────────────────────

/// Every route, in one list.
///
/// This is what the server mounts, and it is the only place that names a path —
/// so the set of paths a client can reach is one screen long, and a route
/// accidentally left unmounted is visible rather than silent.
#[must_use]
pub fn all() -> Vec<Route> {
    routes![
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
        ui,
        preflight,
    ]
}

/// The catchers, in one list, for the same reason.
#[must_use]
pub fn catchers() -> Vec<rocket::Catcher> {
    rocket::catchers![
        not_found,
        unprocessable,
        bad_request,
        unauthorized,
        forbidden,
        too_large,
        unavailable,
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
        assert!(!path_shape_matches("/me/credentials", "me/credentials"));
        assert!(path_shape_matches("/me/credentials", "/me/credentials"));
    }

    #[test]
    fn a_literal_path_matches_only_itself() {
        assert!(path_shape_matches("/api/users", "/api/users"));
        assert!(!path_shape_matches("/api/users", "/api/keys"));
        assert!(!path_shape_matches("/api/users", "/api/users/ada"));
    }

    #[test]
    fn a_parameter_segment_matches_exactly_one_segment() {
        assert!(path_shape_matches("/api/users/<account>", "/api/users/ada"));
        assert!(!path_shape_matches("/api/users/<account>", "/api/users"));
        // One segment, not a tail: `/keys/laptop` must not satisfy `<account>`.
        assert!(!path_shape_matches(
            "/api/users/<account>",
            "/api/users/ada/keys/laptop"
        ));
    }

    #[test]
    fn a_tail_parameter_swallows_the_rest_of_the_path() {
        // The relay and the SPA catch-all both rely on this.
        assert!(path_shape_matches("/<path..>", "/anything/at/all"));
        assert!(path_shape_matches(
            "/relay/anthropic/<sub..>",
            "/relay/anthropic/v1/messages"
        ));
        assert!(path_shape_matches("/<path..>", "/"));
    }

    #[test]
    fn an_unrelated_path_is_not_a_shape_match() {
        // This is the case that must stay a 404: claiming a path exists when it
        // does not is worse than the 405 this is trying to preserve.
        assert!(!path_shape_matches("/api/users", "/nope"));
        assert!(!path_shape_matches("/relay/anthropic/<sub..>", "/api/users"));
    }

    /// The routes that can prove a path exists: everything but the catch-alls.
    fn concrete() -> Vec<String> {
        all()
            .iter()
            .map(|r| r.uri.path().to_string())
            .filter(|p| !is_catch_all(p))
            .collect()
    }

    #[test]
    fn a_wrong_verb_on_a_real_path_is_told_apart_from_an_unknown_path() {
        // The whole reason the helper exists: `/me/credentials` is POST only, so
        // a GET must be answered 405 rather than 404.
        assert!(
            concrete().iter().any(|p| path_shape_matches(p, "/me/credentials")),
            "/me/credentials is mounted"
        );
        assert!(
            !concrete().iter().any(|p| path_shape_matches(p, "/me/nonexistent")),
            "an unmounted path must not be claimed"
        );
    }

    #[test]
    fn the_spa_catch_all_does_not_make_every_path_look_served() {
        // The trap this guards: the SPA route matches everything, so counting it
        // would turn every 404 into a 405 claiming the path exists.
        assert!(is_catch_all("/<path..>"));
        assert!(is_catch_all("/relay/anthropic/<sub..>"));
        assert!(!is_catch_all("/api/users"));
        assert!(!is_catch_all("/api/users/<account>"));
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
