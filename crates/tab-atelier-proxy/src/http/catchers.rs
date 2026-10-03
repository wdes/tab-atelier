// SPDX-License-Identifier: MPL-2.0

//! What the client sees when a request never reaches a controller.
//!
//! Every refusal in this API is the same shape — `{"error": "..."}` with the
//! status that says how the caller should react — and these are the statuses
//! that a guard, Rocket's own routing, or the body reader can produce. Each one
//! exists because the alternative is Rocket's HTML error page, which a JSON
//! client cannot read and which would leak the same information in a second,
//! inconsistent format.
//!
//! The wording comes from the request when a guard left it (see
//! [`crate::http::refusal`]) and from the fallback otherwise, which is the case
//! for the 404 and 405 a request gets for a path or verb nobody mounted.

use rocket::Request;
use rocket::http::Status;

use crate::http::refusal::{Refusal, recall};

/// The body reader could not read the request body.
#[rocket::catch(400)]
pub fn bad_request(req: &Request<'_>) -> Refusal {
    recall(req, Status::BadRequest, "the request could not be read")
}

/// The `Authorization` or `x-api-key` header is missing or wrong.
#[rocket::catch(401)]
pub fn unauthorized(req: &Request<'_>) -> Refusal {
    recall(req, Status::Unauthorized, "the proxy token is wrong")
}

/// The credential is valid but not allowed to do this.
#[rocket::catch(403)]
pub fn forbidden(req: &Request<'_>) -> Refusal {
    recall(req, Status::Forbidden, "this account may not do that")
}

/// No route matches. Rocket reports this by trying the following route, so by
/// the time it reaches here every candidate has been exhausted.
#[rocket::catch(404)]
pub fn not_found(_req: &Request<'_>) -> Refusal {
    Refusal::new(Status::NotFound, "no route matches this path")
}

/// The path exists but not for this verb.
#[rocket::catch(405)]
pub fn method_not_allowed(_req: &Request<'_>) -> Refusal {
    Refusal::new(Status::MethodNotAllowed, "this verb is not allowed on this path")
}

/// A body large enough to be refused for its size alone.
///
/// The expected case is a prompt with a very large pasted file. The limit is
/// per-route (see the `data` attribute on the relay), so a bigger body that
/// arrives on a route that accepts it is not an error.
#[rocket::catch(413)]
pub fn payload_too_large(_req: &Request<'_>) -> Refusal {
    Refusal::new(
        Status::PayloadTooLarge,
        "the request body is larger than this route accepts",
    )
}

/// Rocket's own JSON reader rejected the body before a request struct saw it.
#[rocket::catch(422)]
pub fn unprocessable(req: &Request<'_>) -> Refusal {
    recall(req, Status::UnprocessableEntity, "the request body is not valid JSON")
}

/// No upstream provider can take the request.
#[rocket::catch(503)]
pub fn unavailable(req: &Request<'_>) -> Refusal {
    recall(req, Status::ServiceUnavailable, "no provider is available")
}

/// The list of catchers to register. One place, so adding a status to
/// [`crate::http::refusal`] cannot be forgotten at the mount.
#[must_use]
pub fn all() -> Vec<rocket::Catcher> {
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
    use super::*;

    #[test]
    fn every_status_a_guard_can_choose_has_a_catcher() {
        // A guard that fails with a status nothing catches falls back to
        // Rocket's HTML page, which is the thing these exist to prevent.
        let caught: Vec<Option<u16>> = all().iter().map(|c| c.code).collect();
        for status in [400, 401, 403, 404, 405, 413, 422, 503] {
            assert!(
                caught.contains(&Some(status)),
                "no catcher for {status}, so a guard choosing it would answer HTML"
            );
        }
    }
}
