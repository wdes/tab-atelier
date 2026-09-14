// SPDX-License-Identifier: MPL-2.0

//! Why a request was refused, and how that reason survives the trip to the
//! client.
//!
//! A middleware guard runs before any controller sees the request, and all a
//! guard gets to choose is a [`Status`] — Rocket's route codegen turns a
//! guard's `Outcome::Error` into a response and drops the error value on the
//! way (`rocket_codegen`'s `route` codegen matches `Outcome::Error(_)`). Only a
//! `#[catch]` for that status can build the body, and by then the guard's
//! message is gone.
//!
//! So a guard does two things: it leaves the reason on the request, and it
//! fails with the status. The catcher picks the reason back up. One place
//! decides what a refusal looks like, one place writes it, one place reads it —
//! and the status a guard chose is the status the client sees.

use std::sync::Mutex;

use rocket::http::{ContentType, Status};
use rocket::request::Request;
use rocket::response::{self, Responder, Response};

use crate::http::resources::status::ProblemResource;

/// A refusal with the operator-facing wording attached.
#[derive(Debug, Clone)]
pub struct Refusal {
    /// What the caller is told.
    pub status: Status,
    /// What went wrong, addressed to whoever has to fix it. Not a code: the
    /// caller's next move is always the same — show it — and the machine is
    /// already reading the status line.
    pub message: String,
}

impl Refusal {
    /// A refusal to send.
    #[must_use]
    pub fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// A failure that should not happen: a guard asked for the state and the
    /// state guard itself failed. Says so rather than sending an empty 500, so
    /// the one time it does happen the log names it.
    #[must_use]
    pub fn unmanaged() -> Self {
        Self::new(
            Status::InternalServerError,
            "the proxy could not read its own state — this is a bug, please report it",
        )
    }

    /// The body, in the one error shape this API uses.
    #[must_use]
    pub fn body(&self) -> String {
        serde_json::to_string(&ProblemResource::of(self.message.clone()))
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize failed: {e}"}}"#))
    }
}

impl<'r> Responder<'r, 'static> for Refusal {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let body = self.body();
        Response::build()
            .status(self.status)
            .header(ContentType::JSON)
            .sized_body(body.len(), std::io::Cursor::new(body))
            .ok()
    }
}

/// Whether two secrets are equal, in time that does not depend on where they
/// first differ.
///
/// A byte-wise comparison returns on the first mismatch, so how long it takes
/// reveals how many leading bytes were right — enough to recover a token one
/// byte at a time. This walks the whole of both inputs, and folds the lengths
/// in rather than comparing them first, so neither the content nor the length
/// is observable.
#[must_use]
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        // Out of range reads as 0 rather than short-circuiting: the loop always
        // runs its full length, whatever the inputs.
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// The slot a guard writes its reason into.
///
/// Per-request storage rather than a global: two refusals in flight must not
/// see each other's wording.
#[derive(Default)]
struct Slot(Mutex<Option<Refusal>>);

/// Leave a reason for the catcher that will render it.
///
/// Call this immediately before failing with `refusal.status`.
pub fn remember(req: &Request<'_>, refusal: &Refusal) {
    let slot = req.local_cache(Slot::default);
    let mut held = slot.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *held = Some(refusal.clone());
}

/// What a guard left, or `fallback` when the response came from Rocket itself.
///
/// The fallback matters: a 404 for a path nobody mounted never went past a
/// guard, so there is no stored reason and the catcher has to supply one.
#[must_use]
pub fn recall(req: &Request<'_>, status: Status, fallback: &str) -> Refusal {
    let slot = req.local_cache(Slot::default);
    let held = slot.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    held.clone().unwrap_or_else(|| Refusal::new(status, fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_problem_body_is_the_one_error_shape() {
        let refusal = Refusal::new(Status::BadRequest, "weight must be 1..=100");
        assert_eq!(refusal.body(), r#"{"error":"weight must be 1..=100"}"#);
    }

    #[test]
    fn equal_tokens_match_and_a_difference_anywhere_is_found() {
        assert!(tokens_match("s3cret", "s3cret"));
        // First, middle and last: an early-return comparison is
        // distinguishable on the first of these.
        assert!(!tokens_match("X3cret", "s3cret"));
        assert!(!tokens_match("s3Xret", "s3cret"));
        assert!(!tokens_match("s3creX", "s3cret"));
    }

    #[test]
    fn a_token_is_not_a_prefix_and_length_counts() {
        assert!(!tokens_match("s3cre", "s3cret"));
        assert!(!tokens_match("s3cret", "s3cret-longer"));
        assert!(tokens_match("", ""));
    }

    #[test]
    fn a_refusal_keeps_the_status_it_was_asked_for() {
        assert_eq!(Refusal::new(Status::Unauthorized, "no").status, Status::Unauthorized);
        assert_eq!(
            Refusal::new(Status::ServiceUnavailable, "no").status,
            Status::ServiceUnavailable
        );
    }
}
