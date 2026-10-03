// SPDX-License-Identifier: MPL-2.0

//! The middleware: the decisions a request passes through before a controller
//! sees it.
//!
//! Each module is one decision, and each exposes a plain function over the
//! thing it needs — headers, a credential, the state. The Rocket guards in
//! [`crate::http::guards`] are the thin layer that turns each of these into
//! something a route can demand as an argument.
//!
//! They are functions rather than tower layers on purpose. There are four, they
//! are not composable in a useful way, and the order that matters ("is this
//! authenticated" before "may this account relay") is clearer read as the
//! argument list of a handler than expressed as a stack.

pub(crate) mod admin_token;
pub(crate) mod arrival;
pub(crate) mod user_key;

pub(crate) use user_key::authenticate_and_stamp;
