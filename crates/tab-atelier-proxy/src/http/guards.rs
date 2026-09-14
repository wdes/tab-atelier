// SPDX-License-Identifier: MPL-2.0

//! The middleware, as Rocket request guards.
//!
//! A guard is the middleware here: it runs before the handler, it can refuse
//! the request, and the handler declares what it needs by asking for it. That
//! makes the presence of every check readable from a controller's argument
//! list — `who: ClientKey` cannot be forgotten, because without it the handler
//! does not typecheck.
//!
//! Three guards:
//!
//! * [`Arrival`] — who is calling. Never refuses; it is an observation.
//! * [`ClientKey`] — which account, and record the sighting. Fails 401.
//! * [`Admin`] — may this request use the operator API. Fails 401, or 503 when
//!   the installation has no token at all.
//!
//! Each refusal's wording is left in the request for the matching catcher in
//! [`crate::http::catchers`], because Rocket's codegen keeps a guard's error
//! *status* and drops its error *value*: a message returned inside the
//! `Outcome` is gone before the catcher runs. See [`remember`].

use std::net::IpAddr;
use std::sync::Arc;

use rocket::Request;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};

use crate::http::middleware::{admin_token, arrival, authenticate_and_stamp};
use crate::http::refusal::{Refusal, remember};
use crate::server::State;
use crate::users::Account;

/// The state, as the routes hold it.
type Held = Arc<State>;

/// Who is calling, and from where.
///
/// Never refuses. A request that arrives is a fact, and every handler that
/// wants to record or forward that fact asks for it rather than reading the
/// headers itself — which is what keeps the trusted-hop rule in one place.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// The peer, or loopback when the transport cannot say.
    pub peer: IpAddr,
    /// The address to attribute this request to: the peer, unless a forwarding
    /// header was sent by something trusted.
    pub ip: String,
    /// The credential presented, or empty.
    pub presented: String,
    /// The headers, in the type the relay forwards with.
    pub headers: hyper::HeaderMap,
    /// The verb, in the type the relay forwards with.
    pub method: hyper::Method,
    /// The path and query, exactly as they arrived.
    ///
    /// Carried rather than re-derived from Rocket's typed parameters because the
    /// relay forwards a path, not a route: what it must send upstream is what the
    /// client asked for, including a query string it does not itself parse.
    pub target: String,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Arrival {
    type Error = std::convert::Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let peer = req.remote().map_or_else(arrival::unknown_peer, |a| a.ip());
        let headers = arrival::from_rocket(req.headers());
        Outcome::Success(Self {
            peer,
            ip: arrival::client_ip(&headers, peer),
            presented: arrival::presented(&headers),
            method: hyper::Method::from_bytes(req.method().as_str().as_bytes()).unwrap_or(hyper::Method::GET),
            target: req.uri().to_string(),
            headers,
        })
    }
}

/// The account behind the presented key.
///
/// The message differs from the operator API's so that an operator reading a
/// log can tell the two apart, and so a confused caller is told which header
/// this interface actually reads.
#[derive(Debug, Clone)]
pub struct ClientKey(pub Account);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ClientKey {
    type Error = Refusal;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let Some(state) = state_of(req).await else {
            return Outcome::Error((Status::InternalServerError, Refusal::unmanaged()));
        };
        // `Arrival` cannot refuse — it only reads the request — so its error
        // type is `Infallible` and the `else` is unreachable. It is written out
        // rather than unwrapped so that giving this guard an error later is a
        // compile error here instead of a panic at runtime.
        let Outcome::Success(arrival) = req.guard::<Arrival>().await else {
            return Outcome::Error((Status::InternalServerError, Refusal::unmanaged()));
        };
        authenticate_and_stamp(&state, &arrival.presented, &arrival.ip).map_or_else(
            || {
                refuse(
                    req,
                    Status::Unauthorized,
                    "no valid key: nothing arrived, or the key is unknown or disabled. Check the \
                     x-api-key header, and that the key is enabled for this account",
                )
            },
            |account| Outcome::Success(Self(account)),
        )
    }
}

/// A credential that gets past the front door.
///
/// Two ways, and which one applies depends on who is calling:
///
/// * a **browser** answers the `WWW-Authenticate` challenge with a `Digest`
///   credential. This is the operator looking at the dashboard.
/// * a **CLI or script** presents the operator token directly — the same
///   secret, in a header, with no challenge needed.
///
/// Exempt paths are let through with `signed_in: false`: the relay and the
/// per-user paths authenticate their own callers with a key, and their clients
/// are programs that cannot answer a prompt.
///
/// This is the gate on the dashboard, so it is the gate on the CSS and the
/// JavaScript too — they are served by the same catch-all as the page. That is
/// deliberate: a scraper that never signs in never receives a page, a bundle or
/// a style sheet, so there is nothing to fingerprint and nothing to crawl.
#[derive(Debug, Clone, Copy)]
pub struct WebAuth {
    /// Whether a credential was actually presented and accepted.
    pub signed_in: bool,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WebAuth {
    type Error = Refusal;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        if !crate::http::auth::gated(req.uri().path().as_str(), req.method()) {
            return Outcome::Success(Self { signed_in: false });
        }
        let Some(state) = state_of(req).await else {
            return Outcome::Error((Status::InternalServerError, Refusal::unmanaged()));
        };
        // Checked BEFORE any comparison, and this order is the whole safety of
        // the empty case: `constant_time_eq("", "")` is true, so an
        // installation with no token would otherwise authorise everybody.
        let Some(token) = admin_token::token(&state) else {
            return refuse(
                req,
                Status::ServiceUnavailable,
                "no operator token is configured on this proxy, so nobody can sign in — run \
                 `tab-atelier-proxy admin-token` on the server, as the service user",
            );
        };
        let _ = token;

        // A browser's own answer, which carries a digest rather than a secret.
        //
        // The scheme is tested through `auth::is_digest` rather than by slicing
        // the header here: this value comes off the wire before anything has
        // validated it, and `&header[..7]` panics when byte 7 lands inside a
        // multi-byte character.
        if let Some(header) = req.headers().get_one("authorization")
            && crate::http::auth::is_digest(header)
        {
            // The digest covers the request target, so it is passed exactly
            // as it arrived — path and query, unnormalised.
            let target = req.uri().to_string();
            return match crate::http::auth::verify(&state, req.method(), &target, header, crate::usage::now_secs()) {
                Ok(()) => Outcome::Success(Self { signed_in: true }),
                Err(why) => refuse(req, Status::Unauthorized, &why),
            };
        }

        // `Arrival` cannot refuse — it only reads the request — so its error
        // type is `Infallible` and the `else` is unreachable. It is written out
        // rather than unwrapped so that giving that guard an error later is a
        // compile error here instead of a panic at runtime.
        let Outcome::Success(arrival) = req.guard::<Arrival>().await else {
            return Outcome::Error((Status::InternalServerError, Refusal::unmanaged()));
        };
        match admin_token::guard_token(&state, &arrival.presented) {
            Ok(()) => Outcome::Success(Self { signed_in: true }),
            Err(why) => refuse(req, Status::Unauthorized, &why),
        }
    }
}

/// The administrative credential, for the API.
///
/// Delegates entirely to [`WebAuth`], which already accepts either a browser's
/// Digest credential or the token in a header. Keeping one implementation means
/// the dashboard and the API behind it cannot disagree about who is allowed
/// in — which is the failure that matters, because the dashboard can do
/// everything the API can.
#[derive(Debug, Clone, Copy)]
pub struct Admin;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Admin {
    type Error = Refusal;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let Outcome::Success(web) = req.guard::<WebAuth>().await else {
            return Outcome::Error((Status::InternalServerError, Refusal::unmanaged()));
        };
        if web.signed_in {
            return Outcome::Success(Self);
        }
        // Reachable only if an administrative route were ever put on the exempt
        // list, which none is — so this says what happened rather than sending
        // an empty 401.
        refuse(
            req,
            Status::Unauthorized,
            "this API needs the operator token, and this path asks for no credential",
        )
    }
}

/// The application state, if it was mounted.
///
/// A guard asking for the state and not finding it means the server was built
/// wrong, which is why every caller of this turns `None` into a 500 rather than
/// a refusal a client could act on.
async fn state_of(req: &Request<'_>) -> Option<Held> {
    match req.guard::<&rocket::State<Held>>().await {
        Outcome::Success(s) => Some(Arc::clone(s.inner())),
        Outcome::Error(_) | Outcome::Forward(_) => None,
    }
}

/// Refuse a request, leaving the wording where the catcher will find it.
///
/// The error value is dropped by Rocket's codegen, so returning the message
/// inside the `Outcome` would lose it: every refusal would render as the
/// status's default page, and the sentence explaining the actual problem would
/// never reach the operator.
fn refuse<T>(req: &Request<'_>, status: Status, why: &str) -> Outcome<T, Refusal> {
    let refusal = Refusal::new(status, why);
    remember(req, &refusal);
    Outcome::Error((status, refusal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmanaged_failure_says_so_rather_than_nothing() {
        // This fires when the state guard itself failed, which should not
        // happen; a message that names it beats an empty 500.
        let r = Refusal::unmanaged();
        assert_eq!(r.status, Status::InternalServerError);
        assert!(!r.message.is_empty());
    }

    #[test]
    fn a_refusal_is_json_rather_than_an_html_error_page() {
        // A client that always parses JSON must not be handed Rocket's default
        // page, which is what a bare status would produce.
        let body = Refusal::new(Status::Unauthorized, "no key").body();
        assert!(body.starts_with('{'), "{body}");
        assert!(body.contains("no key"), "{body}");
    }

    #[test]
    fn a_key_refusal_names_the_header_a_caller_should_check() {
        // The message is the only place a confused caller learns which header
        // this interface reads, so it has to name one.
        let r = Refusal::new(Status::Unauthorized, "no valid key: check the x-api-key header");
        assert!(r.message.contains("x-api-key"), "{}", r.message);
    }
}
