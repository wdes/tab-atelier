// SPDX-License-Identifier: MPL-2.0

//! The JSON body guard.
//!
//! One guard, used by every write route: it reads the body and hands it to the
//! request struct's own rules. That is the whole of "a request is a struct that
//! validates itself" — the struct decides what is valid, this decides how a
//! refusal is delivered.
//!
//! The status comes from the refusal, not from the guard. A rule that rejects
//! because a field is missing is a 400; one that rejects because the thing
//! named does not exist is a 404. A single `422` for both would lose the
//! distinction the controllers already made, and every client would have to
//! read the message to tell "you sent nonsense" from "that is not there".
//!
//! Like [`crate::http::raw::Raw`], this is a *data* guard: the body is consumed
//! once, and Rocket's route attribute is what guarantees it is asked for
//! exactly once.

use rocket::Request;
use rocket::data::{Data, FromData, ToByteUnit};
use rocket::http::Status;
use rocket::outcome::Outcome;

use crate::http::refusal::{Refusal, remember};

/// How large a request body may be.
///
/// 64 MiB: an order of magnitude above any prompt that fits in a model's
/// context window, and the size the relay's own capture limits were chosen
/// against. The previous hyper server buffered a body with no cap at all, so
/// this is a tightening rather than a relaxation.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// A full context window is roughly a megabyte of JSON, so anything anywhere
/// near that would start failing long conversations in a way that reads as a
/// client bug rather than a proxy limit. Checked at compile time because a
/// limit is not something a test needs a running process to verify.
const _: () = assert!(MAX_BODY_BYTES >= 32 * 1024 * 1024);

/// A validated request body.
///
/// The inner value has already passed its own `validate`, so a controller
/// never has to ask whether what it is holding is acceptable.
#[derive(Debug)]
pub struct Json<T>(pub T);

/// Turn a refusal's numeric status into Rocket's.
///
/// The request structs carry a `u16` so they stay free of the web framework —
/// they are the API's rules, not Rocket's. This is the one place that has to
/// know both, and it is the boundary, which is where the translation belongs.
const fn status_of(code: u16) -> Status {
    Status::new(code)
}

#[rocket::async_trait]
impl<'r, T> FromData<'r> for Json<T>
where
    T: crate::http::requests::Validated + 'r,
{
    type Error = Refusal;

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> rocket::data::Outcome<'r, Self> {
        let bytes = match data.open(MAX_BODY_BYTES.bytes()).into_bytes().await {
            Ok(body) if body.is_complete() => body.into_inner(),
            Ok(_) => {
                let refusal = Refusal::new(
                    Status::PayloadTooLarge,
                    format!("the request body is larger than the {MAX_BODY_BYTES}-byte limit"),
                );
                remember(req, &refusal);
                return Outcome::Error((refusal.status, refusal));
            }
            Err(e) => {
                let refusal = Refusal::new(Status::BadRequest, format!("the body was cut short: {e}"));
                remember(req, &refusal);
                return Outcome::Error((refusal.status, refusal));
            }
        };
        match T::accept(&bytes::Bytes::from(bytes)) {
            Ok(parsed) => Outcome::Success(Self(parsed)),
            Err(rejected) => {
                let status = status_of(rejected.status);
                let refusal = Refusal::new(status, rejected.message);
                remember(req, &refusal);
                Outcome::Error((status, refusal))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_status_becomes_the_matching_rocket_status() {
        assert_eq!(status_of(404), Status::NotFound);
        assert_eq!(status_of(409), Status::Conflict);
        assert_eq!(status_of(400), Status::BadRequest);
    }

    #[test]
    fn an_unknown_status_is_still_a_status() {
        // The request structs are free to name any code; none of them should
        // panic the guard while turning it into a reply.
        assert_eq!(status_of(499).code, 499);
    }
}
