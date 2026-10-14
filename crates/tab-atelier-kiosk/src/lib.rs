// SPDX-License-Identifier: MPL-2.0

//! The Kiosk web GUI: a small server that sits in front of the daemon.
//!
//! It serves the interface it embeds, and relays the paths it does not own back
//! to the daemon — the rest of the harness routes (`/reports`, `/intent`) and
//! the daemon's own API (`/api/*`) alike — so everything the page calls shares
//! one origin and the browser never makes a cross-origin request.
//!
//! It owns one pane itself. The decision journal is the Kiosk's own data, so the
//! Kiosk reads and writes it directly, behind its own token, rather than asking
//! the daemon for it: a pane that only worked while the daemon was up would be
//! the daemon's pane wearing the Kiosk's name.
//!
//! The modules turn on what changes for its own reason:
//!
//! - [`assets`] — the interface, compiled in so a package cannot be installed
//!   without the files it needs.
//! - [`config`] — the upstream hop, the journal, and the deadlines.
//! - [`route`] — where a request goes: our paths, our panes, or the daemon's.
//! - [`panes`] — the decision board, and the token that opens it.
//! - [`decisions`] — the journal itself, on disk.
//! - [`proxy`] — relaying a request to the daemon and its answer back.
//! - [`server`] — the socket loop that ties them together.
//!
//! The daemon stays the only thing that knows about tabs; this crate reaches it
//! over its HTTP API alone and never imports its types.

pub(crate) mod assets;
pub(crate) mod body;
pub(crate) mod config;
pub(crate) mod decisions;
pub(crate) mod panes;
pub(crate) mod proxy;
pub(crate) mod route;
pub(crate) mod server;

#[cfg(test)]
mod tests;

pub use config::{Config, DEFAULT_BIND, DEFAULT_STATE_DIR, DEFAULT_UPSTREAM};
pub use decisions::Decision;
pub use server::{run, serve};
