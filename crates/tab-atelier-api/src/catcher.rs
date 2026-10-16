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
use rocket::http::Status;

use crate::refusal::{Refusal, recall};

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

/// No route matches. Rocket reports this by trying the following route, so by
/// the time it reaches here every candidate has been exhausted.
#[rocket::catch(404)]
pub fn not_found(req: &Request<'_>) -> Refusal {
    recall(req, Status::NotFound, "no route matches this path")
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
