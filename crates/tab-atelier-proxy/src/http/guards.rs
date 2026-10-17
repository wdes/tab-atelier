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
use tab_atelier_api::rocket_okapi::r#gen::OpenApiGenerator;
use tab_atelier_api::rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};

use crate::http::middleware::{admin_token, arrival, authenticate_and_stamp};
use crate::http::refusal::{Refusal, remember};
use crate::server::State;
use crate::users::Account;

/// The state, as the routes hold it.
type Held = Arc<State>;

/// The proxy's rule for who may call it — the one place it is written.
///
/// Everything structural about authentication (which header to read, how a
/// refusal is remembered, the guard that turns it into a response) is
/// `tab-atelier-api`'s, shared with the kiosk. What is the proxy's own business
/// is the *rule*, and it is here once.
impl tab_atelier_api::auth::Authenticator for State {
    /// The account and key the credential belongs to.
    type Principal = (Account, String);

    fn authenticate(&self, presented: &tab_atelier_api::auth::Presented<'_>) -> Result<Self::Principal, Refusal> {
        // `header.or(bearer)`, and NOT the shared `any()` — which also reads
        // `?token=`. The kiosk accepts a query credential because a plain
        // `<a href>` cannot set a header and its terminal view is opened that
        // way; the proxy must not, because its keys open a paid upstream and a
        // credential in a URL lands in history, logs and `Referer` headers.
        // Sharing the *shape* is not sharing the *rule*.
        let offered = presented.header.or(presented.bearer).unwrap_or_default();
        let store = self.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .authenticate_key(offered)
            .map(|(account, key)| (account.clone(), key.id.clone()))
            .ok_or_else(|| {
                Refusal::new(
                    Status::Unauthorized,
                    "no valid key: nothing arrived, or the key is unknown or disabled. Check the \
                     x-api-key header, and that the key is enabled for this account",
                )
            })
    }

    /// Whether the credential opens the operator surface.
    ///
    /// Two refusals, because they have two different fixes: 503 when the
    /// installation has no admin token at all — a deployment problem, and the
    /// message says how to fix it — and 401 when the presented one is missing
    /// or wrong.
    fn allows_admin(&self, presented: &tab_atelier_api::auth::Presented<'_>) -> Result<(), Refusal> {
        if !admin_token::configured(self) {
            return Err(Refusal::new(
                Status::ServiceUnavailable,
                "no admin token configured — run `tab-atelier-proxy admin-token` on the server, \
                 as the service user, before using this API",
            ));
        }
        // As above: the admin token is a header credential, never a query one.
        let offered = presented.header.or(presented.bearer).unwrap_or_default();
        admin_token::guard_token(self, offered).map_err(|why| Refusal::new(Status::Unauthorized, &why))
    }
}

/// The operator guard, as the shared crate defines it.
///
/// An alias, not a type of its own: every route keeps writing `_admin: Admin`,
/// and the check behind it is the shared guard driving
/// [`Authenticator::allows_admin`] above — so the proxy and the kiosk refuse an
/// operator the same way.
/// Note the parameter: `State`, not `Held`. The shared guard wraps the
/// authenticator in an `Arc` itself (`&State<Arc<A>>`), and the proxy manages
/// exactly that — so `Admin<State>` reads the managed `Arc<State>`. Spelling it
/// `Admin<Held>` asks for an `Arc<Arc<State>>` and every guarded route answers
/// 500 "unmanaged".
pub type Admin = tab_atelier_api::guard::Admin<State>;

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
    pub headers: http::HeaderMap,
    /// The verb, in the type the relay forwards with.
    pub method: http::Method,
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
            method: http::Method::from_bytes(req.method().as_str().as_bytes()).unwrap_or(http::Method::GET),
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
/// The key that authenticated, and the account behind it.
///
/// The key id is carried, not just the account, because the sighting recorded
/// by this guard describes the KEY — and the session a request arrived from is
/// part of that sighting. The relay needs the id to attach a session, and the
/// guard is the last place that knows which of an account's keys was presented.
#[derive(Debug, Clone)]
pub struct ClientKey {
    pub account: Account,
    pub key_id: String,
}

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
            |(account, key_id)| Outcome::Success(Self { account, key_id }),
        )
    }
}

/// Documenting the middleware, so a guarded route may carry `#[openapi]`.
///
/// `Arrival` reads the request, `ClientKey` and `Admin` refuse it, but none of
/// them is a *parameter* of the route — the credential is one header the whole
/// API shares, described once in the document's security section, and the
/// client's address is not something a caller sends on purpose. So each
/// declares no per-route input. What okapi needs is only to know the guard
/// exists: without these impls `#[openapi]` refuses any handler that takes one,
/// which is every route behind a check.
macro_rules! documents_itself {
    ($($guard:ty),+ $(,)?) => {
        $(
            impl OpenApiFromRequest<'_> for $guard {
                fn from_request_input(
                    _g: &mut OpenApiGenerator,
                    _name: String,
                    _required: bool,
                ) -> tab_atelier_api::rocket_okapi::Result<RequestHeaderInput> {
                    Ok(RequestHeaderInput::None)
                }
            }
        )+
    };
}

// `Admin` is not here: it is the shared crate's guard now, and okapi's impl
// for it lives there, once, for every server.
documents_itself!(Arrival, ClientKey);

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
