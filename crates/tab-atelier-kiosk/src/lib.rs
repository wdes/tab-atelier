// SPDX-License-Identifier: MPL-2.0

//! The Kiosk web GUI: its embedded assets, and the routes that serve them.
//!
//! The Kiosk panes (decision, report, intent) are the daemon's routes today,
//! and this crate does not answer them yet. What is here is the interface
//! itself, served from the crate so a package cannot be installed without the
//! files it needs.
//!
//! Split by what changes for its own reason: [`assets`] is the interface
//! compiled in, [`route`] is the path table and the response it builds, and
//! [`server`] is the socket loop that ties the two together. The split is here
//! so the panes can arrive as routes without the accept loop and the asset
//! constants growing into the same file.

mod assets;
mod route;
mod server;

pub use server::{serve, serve_on};
