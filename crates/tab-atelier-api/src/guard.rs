// SPDX-License-Identifier: MPL-2.0

//! The request guards every server gates its API with.
//!
//! [`Actor`] names the caller; [`Admin`] additionally requires the operator
//! surface. Both are generic over the server's [`Authenticator`], so the guard,
//! its refusal and its place in the route are the same in every server — only
//! the rule differs, and the rule is one impl.
//!
//! A refused guard **remembers its reason** before failing, because Rocket's
//! codegen drops a guard's error value and keeps only the status; the catcher
//! ([`crate::catcher`]) reads the reason back. A guard that skipped that step
//! would still refuse, but the client would get a generic message instead of the
//! one that says what to fix.

use std::marker::PhantomData;
use std::sync::Arc;

use rocket::State;
// `request::Outcome` is the two-parameter alias a guard returns; the bare
// `outcome::Outcome` takes three.
use rocket::request::{FromRequest, Outcome, Request};

use crate::auth::{Authenticator, Presented};
use crate::refusal::{Refusal, remember};
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};

/// The credential a request presented, read from every form this API accepts.
///
/// Precedence is `x-api-key`, then `Authorization: Bearer`, then `?token=` — the
/// most explicit first, so a request that somehow carries two is judged by the
/// one a caller set deliberately.
///
/// The query form is accepted at all because a plain `<a href>` and a link
/// pasted into a browser cannot set a header; the kiosk opens a tab's terminal
/// that way, and it has to authorize.
#[must_use]
pub fn presented<'r>(req: &'r Request<'_>) -> Presented<'r> {
    let header = req.headers().get_one("x-api-key");
    let bearer = req
        .headers()
        .get_one("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let query = req.uri().query().and_then(|query| {
        query
            .as_str()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == "token")
            .map(|(_, value)| value)
    });
    Presented { header, bearer, query }
}

/// The caller, once its credential has verified.
///
/// A handler that needs to know who it is serving takes this. The principal is
/// the server's own type — the proxy's account, the kiosk's nothing.
#[derive(Debug)]
pub struct Actor<A: Authenticator>(pub A::Principal);

/// A caller that may use the operator surface.
///
/// A separate guard rather than a method on [`Actor`] so an operator route says
/// so in its signature and nothing else has to remember which routes are
/// privileged.
#[derive(Debug)]
pub struct Admin<A: Authenticator>(PhantomData<A>);

/// Fetch the server's authenticator, or fail as a server bug rather than a
/// refusal the client could act on.
///
/// A missing state is not a credential problem: it means the route was mounted
/// on a server that never managed the authenticator. That is a 500, and saying
/// so is the difference between an operator reading a log and a client retrying
/// a request that can never succeed.
async fn authenticator<A: Authenticator>(req: &Request<'_>) -> Result<Arc<A>, Refusal> {
    match req.guard::<&State<Arc<A>>>().await {
        Outcome::Success(state) => Ok(Arc::clone(state.inner())),
        _ => Err(Refusal::unmanaged()),
    }
}

/// Fail with a remembered reason — the three guards below all end the same way.
fn refuse(request: &Request<'_>, refusal: Refusal) -> Refusal {
    remember(request, &refusal);
    refusal
}

#[rocket::async_trait]
impl<'r, A: Authenticator> FromRequest<'r> for Actor<A> {
    type Error = Refusal;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let auth = match authenticator::<A>(req).await {
            Ok(auth) => auth,
            Err(refusal) => {
                let status = refusal.status;
                return Outcome::Error((status, refuse(req, refusal)));
            }
        };
        match auth.authenticate(&presented(req)) {
            Ok(principal) => Outcome::Success(Self(principal)),
            Err(refusal) => {
                let status = refusal.status;
                Outcome::Error((status, refuse(req, refusal)))
            }
        }
    }
}

#[rocket::async_trait]
impl<'r, A: Authenticator> FromRequest<'r> for Admin<A> {
    type Error = Refusal;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let auth = match authenticator::<A>(req).await {
            Ok(auth) => auth,
            Err(refusal) => {
                let status = refusal.status;
                return Outcome::Error((status, refuse(req, refusal)));
            }
        };
        match auth.allows_admin(&presented(req)) {
            Ok(()) => Outcome::Success(Self(PhantomData)),
            Err(refusal) => {
                let status = refusal.status;
                Outcome::Error((status, refuse(req, refusal)))
            }
        }
    }
}

/// Documenting the guard so a route may carry `#[openapi]`.
///
/// The credential is a request header the whole API shares — described once, in
/// the document's security section — not a per-route parameter, so a guarded
/// route declares no input of its own here. Okapi only needs to know the guard
/// exists; without this impl `#[openapi]` refuses any handler that takes an
/// `Actor` or an `Admin`, which is every guarded route.
impl<A: Authenticator> OpenApiFromRequest<'_> for Actor<A> {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        Ok(RequestHeaderInput::None)
    }
}

/// Documenting the operator guard, as [`Actor`] is documented.
impl<A: Authenticator> OpenApiFromRequest<'_> for Admin<A> {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        Ok(RequestHeaderInput::None)
    }
}

#[cfg(test)]
mod tests {
    use super::{Actor, Admin};
    use crate::auth::{Authenticator, DenyAll, Presented};
    use rocket::http::Status;
    use rocket::local::blocking::Client;
    use std::sync::Arc;

    /// A resolver whose only rule is "the token is `yes`".
    struct Yes;

    impl Authenticator for Yes {
        type Principal = ();

        fn authenticate(&self, p: &Presented<'_>) -> Result<(), crate::refusal::Refusal> {
            if p.any() == Some("yes") {
                Ok(())
            } else {
                Err(crate::refusal::Refusal::new(
                    Status::Unauthorized,
                    "the token must be `yes`",
                ))
            }
        }
    }

    #[rocket::get("/who")]
    fn who(_a: Actor<Yes>) -> &'static str {
        "in"
    }

    /// A second mount with no authenticator managed, for the "server bug" test.
    /// At module scope, not inside the test: `#[rocket::get]` expands to a
    /// `#[macro_export] macro_rules!`, and nesting that in a function is a
    /// `non_local_definitions` warning the hook denies.
    #[rocket::get("/orphan")]
    fn orphan(_a: Actor<Yes>) -> &'static str {
        "in"
    }

    fn client() -> Client {
        let rocket = rocket::build().manage(Arc::new(Yes)).mount("/", rocket::routes![who]);
        Client::tracked(rocket).expect("rocket")
    }

    #[test]
    fn a_bearer_token_is_read() {
        let c = client();
        let r = c
            .get("/who")
            .header(rocket::http::Header::new("Authorization", "Bearer yes"))
            .dispatch();
        assert_eq!(r.status(), Status::Ok);
    }

    #[test]
    fn a_query_token_is_read_too() {
        // A link cannot set a header, and this is how the kiosk opens a viewer.
        let c = client();
        let r = c.get("/who?token=yes").dispatch();
        assert_eq!(r.status(), Status::Ok);
    }

    #[test]
    fn an_x_api_key_is_read() {
        let c = client();
        let r = c
            .get("/who")
            .header(rocket::http::Header::new("x-api-key", "yes"))
            .dispatch();
        assert_eq!(r.status(), Status::Ok);
    }

    #[test]
    fn no_credential_is_a_refused_guard_not_a_route_miss() {
        // Distinguishing these matters: a route miss is a 404 the caller reads
        // as "wrong path", a guard failure is a 401 they read as "wrong token".
        let c = client();
        assert_eq!(c.get("/who").dispatch().status(), Status::Unauthorized);
        assert_eq!(c.get("/nowhere").dispatch().status(), Status::NotFound);
    }

    #[test]
    fn precedence_prefers_the_header_it_was_written_for() {
        let p = Presented {
            header: Some("a"),
            bearer: Some("b"),
            query: Some("c"),
        };
        assert_eq!(p.any(), Some("a"));
        let p = Presented {
            header: None,
            bearer: Some("b"),
            query: Some("c"),
        };
        assert_eq!(p.any(), Some("b"));
        let p = Presented {
            header: None,
            bearer: None,
            query: Some("c"),
        };
        assert_eq!(p.any(), Some("c"));
        assert_eq!(Presented::default().any(), None);
    }

    #[test]
    fn a_mount_with_no_authenticator_is_a_server_bug() {
        // The route is mounted on a rocket with no `Arc<Yes>` managed: the guard
        // must say 500, not 401, because no client can fix it.
        let rocket = rocket::build().mount("/", rocket::routes![orphan]);
        let c = Client::tracked(rocket).expect("rocket");
        assert_eq!(c.get("/orphan").dispatch().status(), Status::InternalServerError);
    }

    #[test]
    fn the_admin_guard_refuses_everyone_by_default() {
        // `Yes` does not override `allows_admin`, so its default applies — a
        // server that never opts in cannot leave an operator route open.
        let auth = Yes;
        let p = Presented {
            header: None,
            bearer: Some("yes"),
            query: None,
        };
        assert!(auth.allows_admin(&p).is_err());
        // And `DenyAll` accepts nothing, even an empty request.
        assert!(DenyAll.authenticate(&Presented::default()).is_err());
    }

    #[test]
    fn an_admin_guard_is_nameable() {
        // Compile-time: the guard the kiosk and the proxy both name.
        fn _takes_admin(_: Admin<Yes>) {}
        fn _takes_actor(_: Actor<Yes>) {}
        // And the extraction sees the same request the guard does.
        let c = client();
        let r = c.get("/who?token=yes").dispatch();
        assert_eq!(r.status(), Status::Ok);
    }
}
