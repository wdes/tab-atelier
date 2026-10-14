// SPDX-License-Identifier: MPL-2.0

//! Where a request goes: a handful of paths are ours, the rest is the daemon's.

use http::{Method, Request};

use crate::assets;

/// What a request resolves to.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Serve an embedded asset: `(body, content-type)`.
    Asset(&'static str, &'static str),
    /// Everything else: hand it to the upstream daemon.
    Proxy,
}

/// Classifies a request. The classification is the whole point of this crate:
/// the UI's own page and assets are ours, and every call the page makes — the
/// harness routes (`/decisions`, `/reports`, `/intent`, …) and the daemon's own
/// API (`/api/*`) — is the daemon's, relayed on the same origin so the browser
/// never makes a cross-origin request.
pub fn route(req: &Request<()>) -> Route {
    // Only fetches can be ours. A POST/PUT/DELETE aimed at a local path is the
    // daemon's business, never a page fetch.
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Route::Proxy;
    }
    match req.uri().path() {
        "/" | "/kiosk" | "/assets/kiosk.html" => Route::Asset(assets::HTML, "text/html; charset=utf-8"),
        "/assets/kiosk.css" => Route::Asset(assets::CSS, "text/css; charset=utf-8"),
        "/assets/kiosk.js" => Route::Asset(assets::JS, "text/javascript; charset=utf-8"),
        _ => Route::Proxy,
    }
}
