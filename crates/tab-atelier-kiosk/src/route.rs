// SPDX-License-Identifier: MPL-2.0

//! What a request resolves to, and the response it gets.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::{Response, StatusCode};

use crate::assets;

/// What a request resolves to before any handler runs it.
pub enum Route {
    /// Serve these bytes with this content type.
    Asset(&'static str, &'static str),
    /// Not ours. The Kiosk panes are the daemon's routes, and once this crate
    /// answers requests on their behalf this arm becomes the reverse proxy the
    /// dashboard carries today.
    NotFound,
}

/// Which local asset a path names, if any.
///
/// `/` and `/kiosk` are the same page: the former is what a browser asks for,
/// the latter is what a link can be written against without depending on being
/// the root of a host.
#[must_use]
pub fn route(path: &str) -> Route {
    match path {
        "/" | "/kiosk" | "/assets/kiosk.html" => Route::Asset(assets::HTML, "text/html; charset=utf-8"),
        "/assets/kiosk.css" => Route::Asset(assets::CSS, "text/css; charset=utf-8"),
        "/assets/kiosk.js" => Route::Asset(assets::JS, "text/javascript; charset=utf-8"),
        _ => Route::NotFound,
    }
}

/// Builds the response for a resolved route.
#[must_use]
pub fn respond(route: &Route) -> Response<Full<Bytes>> {
    match route {
        Route::Asset(body, content_type) => {
            let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, content_type.parse().expect("static header value"));
            response
        }
        Route::NotFound => {
            let mut response = Response::new(Full::new(Bytes::from_static(b"not found")));
            *response.status_mut() = StatusCode::NOT_FOUND;
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Route, assets, route};

    /// The page and its two companions are served, and nothing else is.
    ///
    /// The point is the second half: an unknown path must not fall through to
    /// the page, because a Kiosk that answers the harness routes with its own
    /// HTML would look like it works while answering nothing.
    #[test]
    fn only_the_interface_paths_are_served() {
        for path in [
            "/",
            "/kiosk",
            "/assets/kiosk.html",
            "/assets/kiosk.css",
            "/assets/kiosk.js",
        ] {
            assert!(matches!(route(path), Route::Asset(..)), "{path} should serve an asset");
        }
        for path in ["/decisions", "/reports", "/intent", "/nope", "/assets/"] {
            assert!(
                matches!(route(path), Route::NotFound),
                "{path} should not be served by this crate yet"
            );
        }
    }

    /// The assets are the compiled-in ones, not empty placeholders.
    ///
    /// Guards the failure `include_str!` cannot catch on its own: an empty file
    /// compiles fine and serves a blank page.
    #[test]
    fn the_assets_have_content() {
        // Case-insensitive: the file writes `<!doctype html>`, and which case a
        // doctype uses is not something this test has an opinion about.
        assert!(
            assets::HTML.to_ascii_lowercase().contains("<!doctype html"),
            "html looks empty"
        );
        assert!(assets::CSS.len() > 1000, "css looks empty");
        assert!(assets::JS.len() > 1000, "js looks empty");
    }
}
