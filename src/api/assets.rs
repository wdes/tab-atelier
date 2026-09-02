// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Public (pre-auth) static serving: the `OpenAPI` spec, the RFC 9727 API
//! catalog, the viewer's vendored + own JS/CSS/font, and the site icons.

use std::io::Write;

use super::{
    APPLE_TOUCH_ICON, FAVICON_ICO, FAVICON_PNG_16, FAVICON_PNG_32, FAVICON_SVG, ICON_PNG_192, ICON_PNG_512, MAIN_CSS,
    MAIN_JS, ROBOTS_TAG, ROBOTS_TXT, SITE_WEBMANIFEST, VENDOR_TERM_SYMBOLS_WOFF2, VENDOR_XTERM_CSS,
    VENDOR_XTERM_JS_SERVED, VENDOR_XTERM_UNICODE11_JS, openapi_spec, respond_with_etag,
};

/// Serve a public static asset when `path` names one, returning `true` when a
/// response was written (the caller must then stop). Runs before the auth gate
/// so a favicon / spec / viewer-asset fetch never needs a token.
pub(super) fn try_serve<W: Write>(
    stream: &mut W,
    method: &str,
    path: &str,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) -> bool {
    // OpenAPI spec — public so tooling (Swagger UI, codegen) can fetch it
    // without a token. Read from the installed /usr/share/doc copy.
    if (method, path) == ("GET", "/openapi.yaml") {
        let spec = openapi_spec();
        respond_with_etag(
            stream,
            200,
            "application/yaml; charset=utf-8",
            spec.as_bytes(),
            accept_gzip,
            if_none_match,
            "Cache-Control: no-cache\r\n",
        );
        return true;
    }
    // RFC 9727 API Catalog at the IANA-registered well-known URI. Returns
    // an RFC 9264 linkset pointing to the OpenAPI description via the RFC
    // 8631 `service-desc` relation, so generic API tooling can discover
    // the spec from the host root. Public (no token).
    if (method, path) == ("GET", "/.well-known/api-catalog") {
        let body = r#"{"linkset":[{"anchor":"/.well-known/api-catalog","service-desc":[{"href":"/openapi.yaml","type":"application/yaml","title":"tab-atelier local API (OpenAPI 3.1)"}]}]}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/linkset+json\r\n{ROBOTS_TAG}Cache-Control: no-cache\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        return true;
    }
    if let (
        "GET",
        "/assets/xterm-6.0.0.js"
        | "/assets/xterm-unicode11-6.0.0.js"
        | "/assets/xterm-6.0.0.css"
        | "/assets/main.js"
        | "/assets/main.css"
        | "/assets/term-symbols.woff2",
    ) = (method, path)
    {
        let (body, ctype): (&[u8], &str) = match path {
            "/assets/xterm-6.0.0.js" => (
                VENDOR_XTERM_JS_SERVED.as_bytes(),
                "application/javascript; charset=utf-8",
            ),
            "/assets/xterm-unicode11-6.0.0.js" => (
                VENDOR_XTERM_UNICODE11_JS.as_bytes(),
                "application/javascript; charset=utf-8",
            ),
            "/assets/xterm-6.0.0.css" => (VENDOR_XTERM_CSS.as_bytes(), "text/css; charset=utf-8"),
            "/assets/main.js" => (MAIN_JS.as_bytes(), "application/javascript; charset=utf-8"),
            "/assets/term-symbols.woff2" => (VENDOR_TERM_SYMBOLS_WOFF2, "font/woff2"),
            _ => (MAIN_CSS.as_bytes(), "text/css; charset=utf-8"),
        };
        // Cache aggressively. xterm-*.{js,css} are version-pinned
        // in the URL path; main.{js,css} get a `?version=<hash>`
        // query string from the viewer HTML. Either way, a new
        // deb publishes new content under a new effective cache
        // key — `immutable` is safe.
        respond_with_etag(
            stream,
            200,
            ctype,
            body,
            accept_gzip,
            if_none_match,
            "Cache-Control: public, max-age=31536000, immutable\r\n",
        );
        return true;
    }

    // Site icons + web metadata. Public (no token) — a favicon/robots request
    // must never 401. Served at the origin root so the browser's automatic
    // `/favicon.ico` / `/apple-touch-icon.png` / `/robots.txt` fetches hit us;
    // the viewer HTML also declares them via `__ASSET_PREFIX__` for sub-path
    // reverse-proxy mounts.
    if method == "GET" {
        let icon: Option<(&[u8], &str, &str)> = match path {
            "/favicon.ico" => Some((FAVICON_ICO, "image/x-icon", "public, max-age=604800")),
            "/favicon.svg" => Some((
                FAVICON_SVG.as_bytes(),
                "image/svg+xml; charset=utf-8",
                "public, max-age=604800",
            )),
            "/favicon-16x16.png" => Some((FAVICON_PNG_16, "image/png", "public, max-age=604800")),
            "/favicon-32x32.png" => Some((FAVICON_PNG_32, "image/png", "public, max-age=604800")),
            "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png" => {
                Some((APPLE_TOUCH_ICON, "image/png", "public, max-age=604800"))
            }
            "/icon-192.png" => Some((ICON_PNG_192, "image/png", "public, max-age=604800")),
            "/icon-512.png" => Some((ICON_PNG_512, "image/png", "public, max-age=604800")),
            "/site.webmanifest" => Some((
                SITE_WEBMANIFEST.as_bytes(),
                "application/manifest+json; charset=utf-8",
                "public, max-age=86400",
            )),
            "/robots.txt" => Some((
                ROBOTS_TXT.as_bytes(),
                "text/plain; charset=utf-8",
                "public, max-age=86400",
            )),
            _ => None,
        };
        if let Some((body, ctype, cache)) = icon {
            respond_with_etag(
                stream,
                200,
                ctype,
                body,
                accept_gzip,
                if_none_match,
                &format!("Cache-Control: {cache}\r\n"),
            );
            return true;
        }
    }
    false
}
