// SPDX-License-Identifier: MPL-2.0

//! Why a request was refused, and how that reason survives the trip to the
//! client.
//!
//! The mechanism is `tab-atelier-api`'s, shared with the kiosk, and is
//! re-exported here so the proxy's guards and catchers keep naming one module.
//!
//! The short version, because it is the reason this is not just a type: Rocket's
//! route codegen turns a guard's `Outcome::Error` into a response and drops the
//! error *value* on the way, keeping only the status. So a guard leaves its
//! wording on the request ([`remember`]) and fails with the status; the catcher
//! picks the wording back up ([`recall`]) and renders it. A guard that skipped
//! the first half would still refuse, but its message would be lost and the
//! client would get a generic one.

pub use tab_atelier_api::refusal::{Refusal, recall, remember, tokens_match};
