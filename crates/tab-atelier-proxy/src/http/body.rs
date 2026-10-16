// SPDX-License-Identifier: MPL-2.0

//! The JSON body guard.
//!
//! The guard is `tab-atelier-api`'s, shared with the kiosk, and re-exported here
//! so the proxy's routes keep naming one module. It reads the body, hands it to
//! the request struct's own rules, and delivers a refusal with the status that
//! rule chose — 400 for a shape it cannot read, whatever the rule named for a
//! body it will not accept.
//!
//! The size limit is [`MAX_BODY_BYTES`], an order of magnitude above any prompt
//! that fits in a model's context window; [`crate::http::config`] passes it to
//! Rocket so the transport rejects an oversize body before the guard runs.

pub use tab_atelier_api::body::{Json, MAX_BODY_BYTES, status_of};
