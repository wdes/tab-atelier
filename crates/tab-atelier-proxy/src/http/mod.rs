// SPDX-License-Identifier: MPL-2.0

//! The HTTP layer: routes, requests, controllers, resources and middleware.
//!
//! Laid out the way a Laravel application is, because that is the shape this
//! codebase is maintained in:
//!
//! * [`routes`] — one table saying which request goes to which controller.
//! * [`requests`] — one struct per body, each of which validates itself, so a
//!   controller only ever sees a value that was already legal.
//! * [`controllers`] — one module per resource, holding the work.
//! * [`resources`] — one module per resource, turning domain types into the
//!   JSON that goes out.
//! * [`middleware`] — the guards a request passes on the way in.
//!
//! The point of the split is that a reader looking for "what happens when
//! somebody adds a key" has one file to open, and a reader looking for "what
//! does the server answer" has one table.

pub mod controllers;
pub mod middleware;
pub mod requests;
pub mod resources;
pub mod routes;

use std::path::PathBuf;

use crate::server::State;
use crate::transport::Reply;

/// The directory the registries live in.
///
/// The registry file names the directory it sits in, so a provider's key and
/// the registry that mentions it cannot drift apart: there is one answer to
/// "where does this live", and it is derived from the path the process was
/// given rather than from the working directory, which a service manager is
/// free to set to anything.
pub(crate) fn registry_dir(state: &State) -> PathBuf {
    state
        .registry_path
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
}

/// A refusal, as every 4xx and 5xx in this tree writes one.
pub(crate) fn problem(status: u16, message: impl Into<String>) -> Reply {
    crate::transport::json_of(status, &resources::status::ProblemResource::of(message))
}

/// An acknowledgement, as every "and it happened" route writes one.
pub(crate) fn acknowledged() -> Reply {
    crate::transport::json_of(200, &resources::status::OkResource::yes())
}
