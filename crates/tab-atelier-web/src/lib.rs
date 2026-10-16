// SPDX-License-Identifier: MPL-2.0

//! The web-serving layer the proxy and kiosk UIs share.
//!
//! Both crates hand-rolled the same thing: resolve a web root, serve a file with
//! its content type, build JSON responses and a JSON error shape, compare a
//! token in constant time. This crate is that, once.
//!
//! # What it is not
//!
//! It does not embed assets — a UI is files served from a web root
//! ([`assets`]), so a rebuilt page is a copy and not a build of the server. It
//! does not route: each server knows its own paths, and only a few really are
//! shared (the static assets). And it holds no UI of its own; the Vue apps live
//! in the crates that serve them.

pub mod assets;
pub mod response;

pub use response::{Body, bytes, error, json, query_token, redirect, token_matches};
