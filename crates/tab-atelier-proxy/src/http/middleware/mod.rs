// SPDX-License-Identifier: MPL-2.0

//! The guards a request passes before a controller sees it.
//!
//! Each is a plain function over `InReq`, not a tower layer. There are three of
//! them, they are not composable, and the order matters in a way that is
//! clearer read top to bottom in `routes::dispatch` than expressed as a stack.

pub(crate) mod admin_token;
pub(crate) mod arrival;
pub(crate) mod reachability;
pub(crate) mod user_key;

pub(crate) use arrival::presented;
pub(crate) use reachability::reachability_probe;
pub(crate) use user_key::authenticate_and_stamp;
