// SPDX-License-Identifier: MPL-2.0

//! The HTTP API a tab-atelier server writes to.
//!
//! This is the chain of the shop's HTTP surface — the proxy's own best
//! practices, with the proxy's business left out — so that every tab-atelier
//! server speaks one dialect: one error shape, one response type, one way to
//! declare a route that also documents it.
//!
//! # The three ideas
//!
//! - **Request** — a body is a struct that validates itself ([`request`]). A
//!   route takes it as an argument; a refusal is a status and a sentence.
//! - **Response** — a handler answers with a [`Reply`]. Its status, headers and
//!   body are the transport's to render, and a server that keeps hyper renders
//!   them itself.
//! - **Resource** — a noun with its own envelope ([`resource`]). The two that
//!   carry no fields are [`OkResource`]'s bare acknowledgement and
//!   [`ProblemResource`], the shape of every 4xx and 5xx this API produces.
//!
//! # Transport-neutral, then Rocket
//!
//! [`marshal`], [`resource`] and [`request`] name no Rocket type, so a server
//! that does not use Rocket can still speak this dialect. [`body`] and
//! [`refusal`] are the boundary where a Rocket data guard and a Rocket responder
//! meet those types; the catcher set and the `OpenAPI` figure build on them.
//!
//! # Re-exports
//!
//! [`rocket`], [`rocket_okapi`] and [`schemars`] are re-exported so an app
//! depends on this crate alone rather than pinning the three-crate lockstep
//! itself.

#![deny(missing_docs)]

pub mod auth;
pub mod body;
pub mod catcher;
pub mod guard;
pub mod marshal;
pub mod refusal;
pub mod request;
pub mod resource;
pub mod respond;

pub use auth::{Authenticator, DenyAll, Presented};
pub use body::{Json, MAX_BODY_BYTES};
pub use catcher::catchers;
pub use guard::{Actor, Admin, presented};
pub use marshal::{Body, Reply, ReplyBody, json, json_of, text};
pub use refusal::{Refusal, recall, remember, tokens_match};
pub use request::{Rejection, Validated, loose_bool, parse};
pub use resource::{OkResource, ProblemResource};
pub use respond::{BodyReader, status_of};

pub use rocket;
pub use rocket_okapi;
pub use schemars;

/// Rocket's configuration for a tab-atelier server.
///
/// The only thing it decides beyond Rocket's defaults is the body limit — the
/// number a data guard enforces. It is a parameter rather than a constant here
/// because the limit is a property of the *server*: an operator API that accepts
/// a prompt and a small UI that accepts a note do not want the same ceiling, and
/// making one app's number the shared default would be the kind of coupling this
/// crate exists to remove.
#[must_use]
pub fn config(addr: std::net::SocketAddr, max_body: u64) -> rocket::Config {
    rocket::Config {
        address: addr.ip(),
        port: addr.port(),
        limits: rocket::data::Limits::default().limit("json", rocket::data::ByteUnit::from(max_body)),
        ..rocket::Config::default()
    }
}

/// A refusal built from a status and a sentence.
///
/// The one way to produce an error response, so every route that refuses looks
/// the same from outside.
#[must_use]
pub fn problem(status: u16, message: &str) -> Reply {
    json_of(status, &ProblemResource::of(message))
}

/// A bare acknowledgement.
#[must_use]
pub fn acknowledged() -> Reply {
    json_of(200, &OkResource::yes())
}
