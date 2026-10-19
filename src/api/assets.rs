// SPDX-License-Identifier: MPL-2.0

//! Public (pre-auth) static serving: the `OpenAPI` spec, the RFC 9727 API
//! catalog, the viewer's vendored + own JS/CSS/font, and the site icons.

use tab_atelier_api::{Conditional, Reply, cached};

use super::{
    APPLE_TOUCH_ICON, FAVICON_ICO, FAVICON_PNG_16, FAVICON_PNG_32, FAVICON_SVG, ICON_PNG_192, ICON_PNG_512, MAIN_CSS,
    MAIN_JS, ROBOTS_TXT, SITE_WEBMANIFEST, VENDOR_TERM_SYMBOLS_WOFF2, VENDOR_XTERM_CSS, VENDOR_XTERM_JS_SERVED,
    VENDOR_XTERM_UNICODE11_JS, openapi_spec,
};

/// A cacheable asset: its bytes, its content type, and how long it may be kept.
///
/// `Copy` because every field is: the three are `'static`, so passing one to
/// [`cacheable`] would otherwise be a move of three pointers for no reason.
#[derive(Clone, Copy)]
struct Asset {
    body: &'static [u8],
    ctype: &'static str,
    cache: &'static str,
}

/// Serve a public static asset when `path` names one.
///
/// Returns `None` when the path is not a public asset, so the caller carries on
/// to the auth gate. Returning a [`Reply`] rather than writing one means a
/// favicon, a spec and the viewer's CSS are all one expression each, and the
/// conditional-GET rules (tag per representation, `Vary`) are the shared ones
/// instead of four hand-written copies.
pub(super) fn serve(method: &str, path: &str, client: Conditional<'_>) -> Option<Reply> {
    if method != "GET" {
        return None;
    }
    // OpenAPI spec — public so tooling (Swagger UI, codegen) can fetch it
    // without a token. Read from the installed /usr/share/doc copy.
    if path == "/openapi.yaml" {
        return Some(cacheable(
            Asset {
                // Leaked once per process by `openapi_spec`, and served for the
                // life of the process; the reply borrows it for exactly as long
                // as the write takes, so a copy here costs one allocation on a
                // route that is fetched once per tool run.
                body: Box::leak(openapi_spec().into_bytes().into_boxed_slice()),
                ctype: "application/yaml; charset=utf-8",
                cache: "no-cache",
            },
            client,
        ));
    }
    // RFC 9727 API Catalog at the IANA-registered well-known URI. Returns
    // an RFC 9264 linkset pointing to the OpenAPI description via the RFC
    // 8631 `service-desc` relation, so generic API tooling can discover
    // the spec from the host root. Public (no token).
    if path == "/.well-known/api-catalog" {
        const BODY: &[u8] = br#"{"linkset":[{"anchor":"/.well-known/api-catalog","service-desc":[{"href":"/openapi.yaml","type":"application/yaml","title":"tab-atelier local API (OpenAPI 3.1)"}]}]}"#;
        return Some(
            Reply::bytes(200, bytes::Bytes::from_static(BODY))
                .with_header("content-type", "application/linkset+json".to_owned())
                .with_header("cache-control", "no-cache".to_owned()),
        );
    }
    if let Some(asset) = viewer_asset(path) {
        return Some(cacheable(asset, client));
    }
    icon(path).map(|asset| cacheable(asset, client))
}

/// The viewer's own and vendored scripts, styles and font.
fn viewer_asset(path: &str) -> Option<Asset> {
    let (body, ctype) = match path {
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
        "/assets/main.css" => (MAIN_CSS.as_bytes(), "text/css; charset=utf-8"),
        "/assets/term-symbols.woff2" => (VENDOR_TERM_SYMBOLS_WOFF2, "font/woff2"),
        _ => return None,
    };
    Some(Asset {
        body,
        ctype,
        // Cache aggressively. xterm-*.{js,css} are version-pinned in the URL
        // path; main.{js,css} get a `?version=<hash>` query string from the
        // viewer HTML. Either way a new deb publishes new content under a new
        // effective cache key, so `immutable` is safe.
        cache: "public, max-age=31536000, immutable",
    })
}

/// The site icons and web metadata.
///
/// Served at the origin root so the browser's automatic `/favicon.ico` /
/// `/apple-touch-icon.png` / `/robots.txt` fetches hit us; the viewer HTML also
/// declares them via `__ASSET_PREFIX__` for sub-path reverse-proxy mounts.
fn icon(path: &str) -> Option<Asset> {
    let (body, ctype, cache) = match path {
        "/favicon.ico" => (FAVICON_ICO, "image/x-icon", "public, max-age=604800"),
        "/favicon.svg" => (
            FAVICON_SVG.as_bytes(),
            "image/svg+xml; charset=utf-8",
            "public, max-age=604800",
        ),
        "/favicon-16x16.png" => (FAVICON_PNG_16, "image/png", "public, max-age=604800"),
        "/favicon-32x32.png" => (FAVICON_PNG_32, "image/png", "public, max-age=604800"),
        "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png" => {
            (APPLE_TOUCH_ICON, "image/png", "public, max-age=604800")
        }
        "/icon-192.png" => (ICON_PNG_192, "image/png", "public, max-age=604800"),
        "/icon-512.png" => (ICON_PNG_512, "image/png", "public, max-age=604800"),
        "/site.webmanifest" => (
            SITE_WEBMANIFEST.as_bytes(),
            "application/manifest+json; charset=utf-8",
            "public, max-age=86400",
        ),
        "/robots.txt" => (
            ROBOTS_TXT.as_bytes(),
            "text/plain; charset=utf-8",
            "public, max-age=86400",
        ),
        _ => return None,
    };
    Some(Asset { body, ctype, cache })
}

/// A conditional reply carrying this asset's cache directive.
fn cacheable(asset: Asset, client: Conditional<'_>) -> Reply {
    let etag = format!("{:08x}", crate::crc32(asset.body));
    cached(200, asset.ctype, bytes::Bytes::from_static(asset.body), &etag, client)
        .with_header("cache-control", asset.cache.to_owned())
}
