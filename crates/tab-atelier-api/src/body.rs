// SPDX-License-Identifier: MPL-2.0

//! The JSON body guard.
//!
//! One guard, used by every write route: it reads the body and hands it to the
//! request struct's own rules. That is the whole of "a request is a struct that
//! validates itself" — the struct decides what is valid, this decides how a
//! refusal is delivered.
//!
//! The status comes from the refusal, not from the guard. A rule that rejects
//! because a field is missing is a 400; one that rejects because the thing named
//! does not exist is a 404. A single 422 for both would lose the distinction the
//! controllers already made, and every client would have to read the message to
//! tell "you sent nonsense" from "that is not there".
//!
//! This is a *data* guard: the body is consumed once, and Rocket's route
//! attribute is what guarantees it is asked for exactly once.

use rocket::Request;
use rocket::data::{Data, FromData, ToByteUnit};
use rocket::http::Status;
use rocket::outcome::Outcome;
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::okapi::Map;
use rocket_okapi::okapi::openapi3::{MediaType, RequestBody};
use rocket_okapi::request::OpenApiFromData;

use crate::refusal::{Refusal, remember};
use crate::request::Validated;
use crate::schemars::JsonSchema;

/// How large a request body may be by default.
///
/// 64 MiB: an order of magnitude above any prompt that fits in a model's context
/// window. A server that wants a smaller cap passes its own to
/// [`crate::config`]; this is the ceiling for a server that just wants the
/// guard to work.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// A full context window is roughly a megabyte of JSON, so anything anywhere
/// near that would start failing long conversations in a way that reads as a
/// client bug rather than a server limit. Checked at compile time because a
/// limit is not something a test needs a running process to verify.
const _: () = assert!(MAX_BODY_BYTES >= 32 * 1024 * 1024);

/// A validated request body.
///
/// The inner value has already passed its own `validate`, so a controller never
/// has to ask whether what it is holding is acceptable.
#[derive(Debug)]
pub struct Json<T>(pub T);

/// So a handler reads `body.field`, as it would from Rocket's own `Json` — the
/// guard is transparent, and the `Validated` impl behind it is the only thing
/// that made it worth not using Rocket's.
impl<T> std::ops::Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// Turn a refusal's numeric status into Rocket's.
///
/// The request structs carry a `u16` so they stay free of the web framework —
/// they are the API's rules, not Rocket's. This is the one place that has to
/// know both, and it is the boundary, which is where the translation belongs.
#[must_use]
pub const fn status_of(code: u16) -> Status {
    Status::new(code)
}

#[rocket::async_trait]
impl<'r, T> FromData<'r> for Json<T>
where
    T: Validated + 'r,
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

/// Documenting the body, so a route may carry `#[openapi]`.
///
/// `Json<T>` is this crate's own guard, not Rocket's, so okapi has no impl for
/// it: without this, a documented route that takes a validated body would refuse
/// to compile, and the only way around — falling back to Rocket's `Json` —
/// would skip the `Validated` contract entirely and answer a malformed body with
/// a 422 the API does not speak. The body is described by `T`'s schema, as JSON.
impl<'r, T: Validated + JsonSchema + 'r> OpenApiFromData<'r> for Json<T> {
    fn request_body(g: &mut OpenApiGenerator) -> rocket_okapi::Result<RequestBody> {
        let schema = g.json_schema::<T>();
        let mut content = Map::new();
        content.insert(
            "application/json".to_owned(),
            MediaType {
                schema: Some(schema),
                ..MediaType::default()
            },
        );
        Ok(RequestBody {
            content,
            required: true,
            ..RequestBody::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::status_of;
    use rocket::http::Status;

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
