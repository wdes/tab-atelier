// SPDX-License-Identifier: MPL-2.0

//! The interface, compiled into the binary.
//!
//! `include_str!` rather than reading from disk at run time, and that is a
//! deployment decision rather than a performance one: a `.deb` that carries a
//! binary and a directory of assets can be installed with one of them missing,
//! and the failure shows up as a blank page in a browser nobody is watching.
//! Compiled in, the two cannot come apart.

/// The page.
pub const HTML: &str = include_str!("../assets/kiosk.html");

/// Its stylesheet.
pub const CSS: &str = include_str!("../assets/kiosk.css");

/// Its script.
pub const JS: &str = include_str!("../assets/kiosk.js");

/// The serveurs & onglets screen.
pub const MANAGE_HTML: &str = include_str!("../assets/manage.html");

/// Its stylesheet.
pub const MANAGE_CSS: &str = include_str!("../assets/manage.css");

/// Its script.
pub const MANAGE_JS: &str = include_str!("../assets/manage.js");
