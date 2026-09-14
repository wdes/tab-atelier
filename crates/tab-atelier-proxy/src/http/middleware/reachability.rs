// SPDX-License-Identifier: MPL-2.0

//! A probe that answers without touching any upstream.
//!
//! The desktop app pings the relay's reachability before a tab starts, and that
//! ping used to authenticate and resolve a provider like any other request —
//! which meant a proxy that was merely slow to reach an upstream looked
//! unreachable, and the app started failing tabs. This answers first.

use hyper::Method;

use crate::transport::{Reply, json};

/// The answer for a probe, if this is one.
///
/// `None` means "not a probe", which is not a failure: the caller carries on to
/// route the request normally.
#[must_use]
pub fn reachability_probe(sub: &str, method: &Method) -> Option<Reply> {
    if sub != "/api/hello" || !matches!(*method, Method::HEAD | Method::GET) {
        return None;
    }
    let body = if *method == Method::HEAD { "" } else { "{}" };
    Some(json(200, body))
}
