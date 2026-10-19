// SPDX-License-Identifier: MPL-2.0

//! The tab-atelier Kiosk: a multi-server control surface over HTTP.
//!
//! The Kiosk serves a small UI and a JSON API that aggregates every configured
//! remote tab-atelier server. It is its own program — it has no privilege on
//! any host, and it holds each remote's token server-side so a browser never
//! sees one.
//!
//! # The HTTP layer comes from `tab-atelier-api`
//!
//! The routes, the request/response/resource contracts, the auth guard, the
//! derived `OpenAPI` and the catchers are `tab-atelier-api`'s; this crate writes
//! only the Kiosk's rules on top. Two of that crate's dependencies are named
//! here as well — `rocket` and `rocket_okapi` — and the public re-export below
//! is why: the attribute macros expand to absolute `::rocket::` and
//! `::rocket_okapi::` paths, and an absolute path resolves only against a crate
//! in this crate's own extern prelude. The re-exported names (see `pub use`
//! below) carry the *types*; the extern dependency carries the *path*.

pub(crate) mod agent;
pub(crate) mod auth;
pub(crate) mod backup;
pub(crate) mod card;
pub(crate) mod config;
pub(crate) mod daemon;
pub(crate) mod intent;
pub(crate) mod panes;
pub(crate) mod remotes;
pub(crate) mod reports;
pub(crate) mod resources;
pub(crate) mod routes;
pub(crate) mod server;

#[cfg(test)]
mod tests;

// The absolute paths the okapi macros emit (`::rocket_okapi::…`) and the
// schemars derive emits (`schemars::…`) resolve through these crate-root names
// as well as through the extern prelude, so a module that only imports this
// crate can still derive a schema.
pub use tab_atelier_api::{rocket, rocket_okapi, schemars};

pub use backup::{BackupError, backup, restore, state_dir};
pub use card::{AgentCard, CARD_KEYS, card_from_meta};
pub use config::{Config, DEFAULT_BIND, DEFAULT_STATE_DIR};
// The journal now lives in its own crate, because the daemon needs the same
// file format and the same two mutations. Re-exported here so every existing
// `crate::Decision` import keeps resolving.
pub use intent::{Intentions, Summary};
pub use remotes::{Remote, RemoteView};
pub use reports::Report;
pub use server::{run, serve};
pub use tab_atelier_decisions::Decision;
// The catalogue moved to its own crate: the Kiosk serves it, the CLI edits it on a box
// with no browser, and both go through this one implementation of the retire rule.
pub use tab_atelier_catalog::{Catalog, CatalogError, EditRequest, Skill};
