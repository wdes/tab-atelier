// SPDX-License-Identifier: MPL-2.0

//! The Kiosk web GUI: a small server that sits in front of the daemon.
//!
//! It serves the interface it embeds, it answers the panes whose data is the
//! operator's own, and it relays everything else to the daemons it aggregates —
//! so every call the page makes shares one origin and the browser never makes a
//! cross-origin request.
//!
//! # What it owns, and what it relays
//!
//! The panes it owns are the ones whose data is the Kiosk's, not a daemon's: the
//! decision board ([`decisions`]), the outbox of reports ([`reports`]), the
//! intentions ([`intent`]), and the list of servers it aggregates ([`remotes`]).
//! Everything else — the daemon's own API under `/api/*` and any harness route
//! it does not answer itself — is relayed ([`proxy`]) to the active upstream.
//! A pane that only worked while a daemon was up would be the daemon's pane
//! wearing the Kiosk's name.
//!
//! # The modules
//!
//! Each turns on what changes for its own reason:
//!
//! - [`assets`] — the interface, compiled in so a package cannot be installed
//!   without the files it needs.
//! - [`config`] — the upstream hop, the journal, the remotes, the token.
//! - [`route`] — where a request goes: our assets, our panes, the daemon's.
//! - [`panes`] — the pane handlers and the token that opens them.
//! - [`decisions`], [`reports`], [`intent`] — the data those panes read and write.
//! - [`remotes`], [`daemon`] — the servers this Kiosk reaches, and the client.
//! - [`proxy`] — relaying a request to the daemon and its answer back.
//! - [`server`] — the socket loop that ties them together.
//! - [`backup`] — a copy of the whole state directory, and a copy back.
//!
//! The daemons stay the only things that know about tabs; this crate reaches
//! each over its HTTP API alone and never imports its types.

pub(crate) mod assets;
pub(crate) mod backup;
pub(crate) mod body;
pub(crate) mod config;
pub(crate) mod daemon;
pub(crate) mod decisions;
pub(crate) mod intent;
pub(crate) mod panes;
pub(crate) mod proxy;
pub(crate) mod remotes;
pub(crate) mod reports;
pub(crate) mod route;
pub(crate) mod server;

#[cfg(test)]
mod tests;

pub use backup::{BackupError, backup, restore, state_dir};
pub use config::{Config, DEFAULT_BIND, DEFAULT_STATE_DIR, DEFAULT_UPSTREAM};
pub use decisions::Decision;
pub use intent::{Intentions, Summary};
pub use remotes::{Remote, RemoteView};
pub use reports::Report;
pub use server::{run, serve};
