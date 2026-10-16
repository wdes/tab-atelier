// SPDX-License-Identifier: MPL-2.0

//! The Kiosk: a web view over several tab-atelier instances at once.
//!
//! It is not a front for one daemon. Each project is independent and its own
//! install; the Kiosk reaches each one over its HTTP API, with that instance's
//! own URL and token, and shows them together. There is no central daemon, and
//! the crate never imports another crate's types — the HTTP API is the whole
//! contract, which is what lets the pieces talk without being welded together.
//!
//! # What it serves
//!
//! Its own interface, and the panes whose data is the operator's: the decision
//! board ([`decisions`]), the outbox of reports ([`reports`]), the intentions
//! ([`intent`]), the skills catalogue, and the fleet of the servers the operator
//! configured ([`remotes`]). Anything else is a 404 — there is no single daemon
//! to relay the rest to.
//!
//! # The modules
//!
//! Each turns on what changes for its own reason:
//!
//! - [`assets`] — the interface, compiled in so a package cannot be installed
//!   without the files it needs.
//! - [`config`] — the bind address, the journal, the remotes, the token.
//! - [`route`] — where a request goes: our assets, our panes, or nothing.
//! - [`panes`] — the pane handlers and the token that opens them.
//! - [`decisions`], [`reports`], [`intent`], [`card`] — the data those panes
//!   read and write.
//! - [`remotes`], [`daemon`] — the servers this Kiosk reaches, and the client
//!   that speaks their API.
//! - [`server`] — the socket loop that ties them together.
//! - [`backup`] — a copy of the whole state directory, and a copy back.

pub(crate) mod backup;
pub(crate) mod body;
pub(crate) mod card;
pub(crate) mod config;
pub(crate) mod daemon;
pub(crate) mod decisions;
pub(crate) mod intent;
pub(crate) mod panes;
pub(crate) mod remotes;
pub(crate) mod reports;
pub(crate) mod route;
pub(crate) mod server;

#[cfg(test)]
mod tests;

pub use backup::{BackupError, backup, restore, state_dir};
pub use card::{AgentCard, CARD_KEYS, card_from_meta};
pub use config::{Config, DEFAULT_BIND, DEFAULT_STATE_DIR};
pub use decisions::Decision;
pub use intent::{Intentions, Summary};
pub use remotes::{Remote, RemoteView};
pub use reports::Report;
pub use server::{run, serve};
