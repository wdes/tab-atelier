// SPDX-License-Identifier: MPL-2.0

//! What the client sees when a request never reaches a handler.
//!
//! Every refusal in this API is one shape — `{"error": "…"}` with the status
//! that says how the caller should react — and these are the statuses a guard,
//! Rocket's own routing, or the body reader can produce. Each exists because the
//! alternative is Rocket's HTML error page, which a JSON client cannot read and
//! which would leak the same information in a second, inconsistent format.
//!
//! Every one of these takes the request and reads it back through
//! [`crate::refusal::recall`], so a guard's own wording survives. A catcher that
//! ignores the request would discard it — which is the bug this set fixes: a
//! refusal that says *what* to change is the difference between a client that
//! can recover and one that can only retry.

use rocket::Request;
use rocket::http::{Method, Status};

use crate::marshal::{Reply, json_of};
use crate::refusal::{Refusal, recall};
use crate::resource::ProblemResource;

/// The body reader could not read the request body.
#[rocket::catch(400)]
pub fn bad_request(req: &Request<'_>) -> Refusal {
    recall(req, Status::BadRequest, "the request could not be read")
}

/// The credential is missing or wrong.
#[rocket::catch(401)]
pub fn unauthorized(req: &Request<'_>) -> Refusal {
    recall(req, Status::Unauthorized, "this credential is not accepted")
}

/// The credential is valid but not allowed to do this.
#[rocket::catch(403)]
pub fn forbidden(req: &Request<'_>) -> Refusal {
    recall(req, Status::Forbidden, "this credential may not do that")
}

/// No route matches — or the path exists under another verb.
///
/// Rocket answers a wrong method with a 404: its router matches the method
/// first, so it cannot tell "no such path" from "that path, another verb", and
/// its matcher is not public. A client probing a path deserves to be told the
/// path exists, and `Allow` says which verbs do — so the distinction is made
/// here, against the mounted routes read back from the running Rocket rather
/// than a second table that could drift.
///
/// Erring towards 404 when unsure is deliberate: a spurious 405 would claim a
/// path exists, a worse lie than the one this fixes. A catch-all is excluded
/// from the scan for the same reason — it matches every path, so counting it
/// would make every unknown path answer 405.
#[rocket::catch(404)]
pub fn not_found(req: &Request<'_>) -> Reply {
    let path = req.uri().path().as_str();
    let mut methods: Vec<String> = req
        .rocket()
        .routes()
        .filter(|route| !is_catch_all(route.uri.path()) && path_shape_matches(route.uri.path(), path))
        .map(|route| route.method.to_string())
        .collect();
    methods.sort();
    methods.dedup();
    if methods.is_empty() {
        let refusal = recall(req, Status::NotFound, "no route matches this path");
        json_of(404, &ProblemResource { error: refusal.message })
    } else {
        json_of(
            405,
            &ProblemResource {
                error: format!(
                    "a {} is not served here; see the OpenAPI document for the verbs",
                    req.method()
                ),
            },
        )
        .with_header("allow", methods.join(", "))
    }
}

/// The refusal a path deserves when it exists under other verbs only.
///
/// `None` means the path is genuinely unknown and the caller decides what that
/// means — a JSON API answers 404, an SPA serves its shell. Exposed because a
/// server's front-end catch-all needs the same judgement as [`not_found`] but
/// holds the routes it mounts rather than a request to read them from, so the
/// matching lives here once.
#[must_use]
pub fn wrong_verb(routes: &[rocket::Route], path: &str, method: Method) -> Option<Reply> {
    let elsewhere = routes.iter().any(|route| {
        route.method != method && !is_catch_all(route.uri.path()) && path_shape_matches(route.uri.path(), path)
    });
    elsewhere.then(|| {
        json_of(
            405,
            &ProblemResource {
                error: format!("a {method} is not served here; see the OpenAPI document for the verbs"),
            },
        )
        .with_header(
            "allow",
            routes
                .iter()
                .filter(|route| path_shape_matches(route.uri.path(), path))
                .map(|route| route.method.to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", "),
        )
    })
}

/// Whether a route pattern is a catch-all — one that matches every path.
///
/// Recognised by its tail wildcard, which is the only form these tables use.
#[must_use]
pub fn is_catch_all(pattern: &str) -> bool {
    pattern
        .split('/')
        .any(|segment| segment.starts_with('<') && segment.ends_with("..>"))
}

/// Whether a route pattern covers a concrete path, ignoring the verb.
///
/// A segment starting with `<` is a parameter, and one ending in `..>` swallows
/// the rest of the path. Both are Rocket's forms; anything else is compared
/// literally.
#[must_use]
pub fn path_shape_matches(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.split('/');
    let mut path = path.split('/');
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            // `<name..>` takes every remaining segment, including none.
            (Some(segment), Some(_)) if segment.starts_with('<') && segment.ends_with("..>") => return true,
            (Some(segment), Some(_)) if segment.starts_with('<') && segment.ends_with('>') => {}
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// The path exists but not for this verb.
#[rocket::catch(405)]
pub fn method_not_allowed(req: &Request<'_>) -> Refusal {
    recall(req, Status::MethodNotAllowed, "this verb is not allowed on this path")
}

/// A body large enough to be refused for its size alone.
#[rocket::catch(413)]
pub fn payload_too_large(req: &Request<'_>) -> Refusal {
    recall(
        req,
        Status::PayloadTooLarge,
        "the request body is larger than this route accepts",
    )
}

/// The body parsed as JSON but did not fit the request struct.
#[rocket::catch(422)]
pub fn unprocessable(req: &Request<'_>) -> Refusal {
    recall(req, Status::UnprocessableEntity, "the request body is not valid JSON")
}

/// A dependency the request needs cannot take it.
#[rocket::catch(503)]
pub fn unavailable(req: &Request<'_>) -> Refusal {
    recall(
        req,
        Status::ServiceUnavailable,
        "the service cannot take the request right now",
    )
}

/// The set to register. One place, so a status a guard can choose cannot be
/// forgotten at the mount.
#[must_use]
pub fn catchers() -> Vec<rocket::Catcher> {
    rocket::catchers![
        bad_request,
        unauthorized,
        forbidden,
        not_found,
        method_not_allowed,
        payload_too_large,
        unprocessable,
        unavailable
    ]
}

#[cfg(test)]
mod tests {
    use super::catchers;

    #[test]
    fn every_status_a_guard_can_choose_has_a_catcher() {
        // A guard that fails with a status nothing catches falls back to
        // Rocket's HTML page, which is the thing these exist to prevent.
        let caught: Vec<Option<u16>> = catchers().iter().map(|c| c.code).collect();
        for status in [400, 401, 403, 404, 405, 413, 422, 503] {
            assert!(
                caught.contains(&Some(status)),
                "no catcher for {status}, so a guard choosing it would answer HTML"
            );
        }
    }
}
