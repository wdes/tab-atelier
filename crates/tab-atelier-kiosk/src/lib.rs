// SPDX-License-Identifier: MPL-2.0

//! The Kiosk web GUI: a small server that sits in front of the daemon.
//!
//! It serves the interface it embeds, and relays every other path to the daemon
//! on the same origin — the harness routes (`/decisions`, `/reports`, `/intent`)
//! and the daemon's own API (`/api/*`) alike. Same origin is the point: the page
//! and the calls it makes then share one origin, so the browser never makes a
//! cross-origin request and CORS never comes up.
//!
//! The modules turn on what changes for its own reason:
//!
//! - [`assets`] — the interface, compiled in so a package cannot be installed
//!   without the files it needs.
//! - [`config`] — the upstream hop and its deadlines.
//! - [`route`] — where a request goes: a handful of paths are ours, the rest is
//!   the daemon's.
//! - [`proxy`] — relaying a request to the daemon and its answer back.
//! - [`server`] — the socket loop that ties them together.
//!
//! The daemon stays the only thing that knows about tabs or decisions; this
//! crate reaches it over its HTTP API alone and never imports its types.

pub(crate) mod assets;
pub(crate) mod body;
pub(crate) mod config;
pub(crate) mod proxy;
pub(crate) mod route;
pub(crate) mod server;

#[cfg(test)]
mod tests;

pub use config::{Config, DEFAULT_BIND, DEFAULT_UPSTREAM};
pub use server::{run, serve};
