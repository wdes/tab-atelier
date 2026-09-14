// SPDX-License-Identifier: MPL-2.0

//! The route table.
//!
//! One place that names every path, the middleware each group passes through,
//! and the handler that answers. Read top to bottom, in the order a request is
//! matched: a path is claimed by the first line that fits it.
//!
//! Handlers do not appear here with the raw request. Each is reached through
//! its [`crate::http::requests`] type, so a handler that takes a body can only
//! be called with a validated one — the parsing and the "no" live in the
//! request struct, and this file is where that promise is enforced rather than
//! merely documented.

use std::sync::Arc;

use hyper::Method;

use crate::http::controllers::{account, inspect, key, mapping, me, provider, relay, usage, web};
use crate::http::middleware::{admin_token, reachability_probe};
use crate::http::requests::Validated;
use crate::http::requests::compact::SetCompact;
use crate::http::requests::inspect::ArmInspect;
use crate::http::requests::key::{AddKey, SetKeyDisabled};
use crate::http::requests::mapping::AddMapping;
use crate::http::requests::provider::{RotateProviderKey, SaveProvider};
use crate::http::requests::user::{AddUser, PinModel, PinProvider, SetDisabled, SetTools, SetWeight};
use crate::server::State;
use crate::transport::{InReq, Reply, json_of, text};

/// Send a request to the handler that owns its path.
pub(crate) async fn dispatch(req: &InReq, state: &Arc<State>) -> Reply {
    let path = req.path.as_str();

    // ── the proxied path, opened by a user key ──────────────────────
    //
    // The reachability probe is answered before the key is looked at, and by
    // exact path: an unauthenticated branch is security surface, and the
    // client that sends this one has no credential to present yet. See
    // [`reachability_probe`].
    if let Some(sub) = path.strip_prefix("/relay/anthropic") {
        if let Some(reply) = reachability_probe(sub, &req.method) {
            return reply;
        }
        return relay::anthropic(req, state).await;
    }

    // ── a person's own key ──────────────────────────────────────────
    //
    // Deliberately not under `/api/`: these are the routes that are meant to
    // be handed to an agent, and the admin API is not.
    if path == "/me/usage" {
        return me::me_usage(req, state);
    }
    if path == "/me/credentials" {
        return me::me_credentials(req, state).await;
    }

    // ── the operator API, opened by the admin token ─────────────────
    if let Some(sub) = path.strip_prefix("/api/") {
        if let Err(refusal) = admin_token::guard(req, state) {
            return refusal;
        }
        return api(req, state, sub);
    }

    // ── the UI, and anything left over ──────────────────────────────
    if req.method == Method::GET || req.method == Method::HEAD {
        return web::web(path, state);
    }
    text(404, "not found")
}

/// `/api/*`, once the admin token has been accepted.
///
/// `sub` is the path with `/api/` already removed.
fn api(req: &InReq, state: &Arc<State>, sub: &str) -> Reply {
    match (req.method.as_str(), sub) {
        // ── reports ─────────────────────────────────────────────────
        // These take their own locks, so they run before anything that holds
        // the account store for a mutation.
        ("GET", "pressure") => crate::http::resources::pressure_json(state),
        ("GET", "providers") => json_of(200, &crate::http::resources::providers_json(state)),
        ("GET", "usage") => usage::usage_report(state, &req.query),
        ("GET", "users") => account::list(state),
        ("GET", hex) if hex.starts_with("inspect") => inspect::status(state),

        // ── inspection ──────────────────────────────────────────────
        ("POST", hex) if hex.starts_with("inspect") => validate::<ArmInspect, _>(req, |r| inspect::arm(r, state)),
        ("DELETE", hex) if hex.starts_with("inspect") => inspect::disarm(state),

        // ── providers ───────────────────────────────────────────────
        ("POST", "providers") => validate::<SaveProvider, _>(req, |r| provider::save(r, state)),
        ("POST", rest) if rest.starts_with("providers/") && rest.ends_with("/key") => {
            let id = rest.trim_start_matches("providers/").trim_end_matches("/key");
            validate::<RotateProviderKey, _>(req, |r| provider::rotate_key(id, r, state))
        }
        ("DELETE", rest) if rest.starts_with("providers/") => {
            provider::remove(rest.trim_start_matches("providers/"), state)
        }

        // ── mappings ────────────────────────────────────────────────
        ("POST", "mappings") => validate::<AddMapping, _>(req, |r| mapping::add(state, r)),
        ("DELETE", rest) if rest.starts_with("mappings/") => {
            mapping::remove(state, rest.trim_start_matches("mappings/"))
        }

        // ── accounts ────────────────────────────────────────────────
        ("POST", "users") => validate::<AddUser, _>(req, |r| account::add(state, r)),

        // Pin an account to a provider, or clear the pin.
        ("POST", rest) if account::pin_path(rest, "provider") => validate::<PinProvider, _>(req, |r| {
            account::set_provider(state, account::pin_who(rest, "provider"), r)
        }),
        // Pin an account to a single model. Outranks the provider pin: choosing
        // a model chooses the hop that serves it.
        ("POST", rest) if account::pin_path(rest, "model") => {
            validate::<PinModel, _>(req, |r| account::set_model(state, account::pin_who(rest, "model"), r))
        }
        // A per-person setting, not a per-provider one: routing picks the hop
        // per request, so a level filed under a provider means something else
        // as soon as the traffic moves. See `account::set_compact` for why the
        // hop still gets a veto.
        ("POST", rest) if rest.ends_with("/compact") => validate::<SetCompact, _>(req, |r| {
            account::set_compact(state, account::pin_who(rest, "compact"), r)
        }),
        // The tool policy is an object, so it is handed over whole rather than
        // flattened to a string and parsed back.
        ("POST", rest) if rest.ends_with("/tools") => {
            validate::<SetTools, _>(req, |r| account::set_tools(state, account::pin_who(rest, "tools"), r))
        }
        ("POST", rest) if rest.ends_with("/disabled") => validate::<SetDisabled, _>(req, |r| {
            account::set_disabled(state, account::pin_who(rest, "disabled"), r)
        }),
        ("POST", rest) if rest.ends_with("/weight") => {
            validate::<SetWeight, _>(req, |r| account::set_weight(state, account::pin_who(rest, "weight"), r))
        }

        // ── keys ────────────────────────────────────────────────────
        // All three go through the same module, which is what keeps "the
        // secret is shown exactly once" checkable in one place.
        ("POST", rest) if rest.ends_with("/keys") => {
            validate::<AddKey, _>(req, |r| key::add(state, account::pin_who(rest, "keys"), r))
        }
        ("POST", rest) if rest.contains("/keys/") && rest.ends_with("/disabled") => {
            validate::<SetKeyDisabled, _>(req, |r| key::set_disabled(state, rest, r))
        }
        ("DELETE", rest) if rest.contains("/keys/") => key::remove(state, rest),

        ("DELETE", rest) if rest.starts_with("users/") => account::remove(state, rest.trim_start_matches("users/")),

        _ => text(404, "not found"),
    }
}

/// Run a handler only if the request validates.
///
/// This is the whole contract in one function: the parser is the first thing
/// the body meets, and the handler is the first thing a valid body meets. A
/// refusal becomes the reply, already phrased by the request struct that
/// produced it.
fn validate<T, F>(req: &InReq, handler: F) -> Reply
where
    T: Validated,
    F: FnOnce(&T) -> Reply,
{
    match T::from_request(req) {
        Ok(parsed) => handler(&parsed),
        Err(refusal) => Reply::from(refusal),
    }
}
