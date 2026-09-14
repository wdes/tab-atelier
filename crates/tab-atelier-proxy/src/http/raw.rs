// SPDX-License-Identifier: MPL-2.0

//! The request body, as bytes.
//!
//! The relay hands the payload to an upstream without looking inside it, and
//! the credential install reads its own envelope, so neither wants a parsed
//! struct. This is the guard both ask for.
//!
//! It is a *data* guard rather than a request guard, which is Rocket's
//! distinction: a request guard prepares an argument, a data guard consumes the
//! body, and only the latter is how a route gets its payload. Declaring it in
//! the route's `data = "..."` is what makes Rocket refuse a second one — a
//! body can only be read once, and the type system is what enforces that.
//!
//! A body limit is declared rather than inherited. Rocket's default is 1 MiB,
//! which a long conversation blows straight through — a 200k-token prompt is
//! already a megabyte of JSON, and the failure would look like a client bug
//! rather than a proxy limit. The number below is generous enough that no
//! legitimate request meets it and small enough that a stream of hostile ones
//! cannot exhaust the machine; the previous hyper server buffered without a cap
//! at all, so this is a tightening, not a relaxation.

use bytes::Bytes;
use rocket::Request;
use rocket::data::{Data, FromData, ToByteUnit};
use rocket::http::Status;
use rocket::outcome::Outcome;

use crate::http::refusal::Refusal;

/// A request body, read whole.
///
/// Whole rather than streamed because both consumers need it that way: the
/// relay buffers to decide how to shape the payload before it opens the
/// upstream connection, and the credential install parses it as JSON.
#[derive(Debug, Clone)]
pub struct Raw(pub Bytes);

impl Raw {
    /// The body as bytes.
    #[must_use]
    pub fn bytes(&self) -> Bytes {
        self.0.clone()
    }

    /// The body as text, for the one caller that logs it.
    #[must_use]
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }
}

/// Refuse a body this guard will not accept.
///
/// Free rather than a method because a data guard's failure type is the same
/// for every request struct, so there is nothing per-type to dispatch on.
fn refuse(status: Status, why: impl Into<String>) -> (Status, Refusal) {
    (status, Refusal::new(status, why))
}

#[rocket::async_trait]
impl<'r> FromData<'r> for Raw {
    type Error = Refusal;

    async fn from_data(_req: &'r Request<'_>, data: Data<'r>) -> rocket::data::Outcome<'r, Self> {
        match data.open(crate::http::body::MAX_BODY_BYTES.bytes()).into_bytes().await {
            Ok(body) if body.is_complete() => Outcome::Success(Self(Bytes::from(body.into_inner()))),
            // The limit was reached, so the body arrived truncated. Reporting
            // success here would forward half a payload upstream and get a
            // confusing parse error back from the provider instead of a clear
            // one from us.
            Ok(_) => Outcome::Error(refuse(
                Status::PayloadTooLarge,
                format!(
                    "the request body is larger than the {}-byte limit",
                    crate::http::body::MAX_BODY_BYTES
                ),
            )),
            Err(e) => Outcome::Error(refuse(Status::BadRequest, format!("the body was cut short: {e}"))),
        }
    }
}

/// The body is empty when there is none.
///
/// A relayed GET has no payload, and the relay still wants to hand something to
/// its forwarding code rather than an `Option` it would have to unwrap at every
/// call site.
#[must_use]
pub const fn empty() -> Raw {
    Raw(Bytes::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_body_is_still_a_body() {
        assert!(empty().bytes().is_empty());
        assert_eq!(empty().text(), "");
    }

    #[test]
    fn the_raw_guard_hands_out_a_copy_of_its_bytes() {
        let raw = Raw(Bytes::from_static(b"{\"a\":1}"));
        assert_eq!(raw.bytes(), Bytes::from_static(b"{\"a\":1}"));
        assert_eq!(raw.text(), "{\"a\":1}");
    }

    #[test]
    fn invalid_utf8_does_not_panic_the_logging_path() {
        // A body that is not text still has to be forwardable, and anything
        // that logs it must not take the process down.
        let raw = Raw(Bytes::from_static(&[0xff, 0xfe]));
        assert!(!raw.text().is_empty());
    }

    #[test]
    fn a_refusal_names_the_limit_it_enforced() {
        let (status, refusal) = refuse(
            Status::PayloadTooLarge,
            format!("over {}", crate::http::body::MAX_BODY_BYTES),
        );
        assert_eq!(status, Status::PayloadTooLarge);
        assert!(refusal.message.contains("67108864"), "{}", refusal.message);
    }
}
