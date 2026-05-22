// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Static assets for the in-browser UI.
//!
//! Files live in `crates/happier-relay/web/` and are embedded into the
//! binary via `include_str!`. No runtime path dependency, no `ServeDir`
//! — the relay is happy to run from `/usr/bin` with no surrounding
//! files. We keep the asset set tiny on purpose (three files); the
//! moment we need a real bundler, we revisit.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

fn served(body: &'static str, mime: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            // Aggressive caching is wrong for a dev UI we ship inline;
            // the assets change with the binary version, so let the
            // client revalidate every load.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub async fn index() -> Response {
    served(INDEX_HTML, "text/html; charset=utf-8")
}
pub async fn app_js() -> Response {
    served(APP_JS, "application/javascript; charset=utf-8")
}
pub async fn style_css() -> Response {
    served(STYLE_CSS, "text/css; charset=utf-8")
}
