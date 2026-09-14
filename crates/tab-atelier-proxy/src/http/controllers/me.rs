// SPDX-License-Identifier: MPL-2.0

//! What a person can see about their own key.
//!
//! Both routes here identify the caller from the key they present rather than
//! from a path segment, which is the whole difference from
//! [`crate::http::controllers::account`]: an operator asking about somebody else
//! goes through the admin guard, and a person asking about themselves presents
//! the key they already have.

use std::sync::Arc;

use crate::http::middleware::arrival::{client_ip, presented};
use crate::http::middleware::authenticate_and_stamp;
use crate::http::resources;
use crate::server::State;
use crate::transport::{InReq, Reply, json_of};
use crate::{egress, usage};
use hyper::Method;

/// The caller's own token totals.
pub(crate) fn me_usage(req: &InReq, state: &Arc<State>) -> Reply {
    if req.method != Method::GET {
        return crate::http::problem(405, "GET only");
    }
    let key = presented(req);
    // Reading your own statistics is a use of the key like any other, and an
    // agent polling this is exactly the traffic someone reviewing access wants
    // to see.
    let Some(account) = authenticate_and_stamp(state, &key, &client_ip(&req.headers, req.peer)) else {
        return refusal();
    };
    let now = usage::now_secs();
    let window = resources::path_window(&req.query).unwrap_or(usage::Window::Hours(24 * 7));
    let usage = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        resources::UsageResource::of(&u, &account.id, window.span(now), now)
    };
    json_of(
        200,
        &resources::MeUsageResource {
            account: resources::AccountSummary {
                name: account.display_name(),
                email: account.email,
            },
            usage,
        },
    )
}

/// Install a repaired credential, on behalf of the caller.
///
/// The key is checked before the body is read, so an unauthenticated caller
/// cannot use the shape of the reply to learn whether a credential would have
/// been accepted.
pub(crate) async fn me_credentials(req: &InReq, state: &Arc<State>) -> Reply {
    if req.method != Method::POST {
        return crate::http::problem(405, "POST only");
    }
    let key = presented(req);
    let ip = client_ip(&req.headers, req.peer);
    let Some(account) = authenticate_and_stamp(state, &key, &ip) else {
        return refusal();
    };
    // The edge already read the body into memory, so there is nothing to
    // collect here — only to decode.
    let Ok(raw) = String::from_utf8(req.body.to_vec()) else {
        return crate::http::problem(400, "body is not UTF-8");
    };

    // Blocking: it makes up to two calls to Anthropic.
    let outcome = tokio::task::spawn_blocking(move || egress::repair_credentials(&raw)).await;
    match outcome {
        Ok(Ok(imported)) => {
            log::warn!(
                "claude credential replaced via /me/credentials by {} <{}> from {ip}",
                account.display_name(),
                account.email,
            );
            let who = imported.identity.map(|i| i.email).unwrap_or_default();
            json_of(200, &resources::CredentialsResource::installed(who))
        }
        Ok(Err(e)) => {
            log::warn!(
                "credential repair refused for {} <{}> from {ip}: {e}",
                account.display_name(),
                account.email,
            );
            // 409: the request was well-formed and authenticated, the state
            // just does not permit it — a working proxy, or somebody else's
            // account. Distinct from 400 so a client can tell "nothing to fix"
            // from "you sent rubbish".
            crate::http::problem(409, e)
        }
        Err(_) => crate::http::problem(500, "credential repair panicked"),
    }
}

/// The one refusal both routes give, so a client cannot tell them apart.
fn refusal() -> Reply {
    crate::http::problem(401, "present your proxy key (x-api-key or Authorization: Bearer)")
}
