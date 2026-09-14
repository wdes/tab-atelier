// SPDX-License-Identifier: MPL-2.0

//! The operator UI, served from the same origin as the API.
//!
//! Assets are embedded in the binary rather than read from disk: the proxy is
//! deployed as a single file, and a missing asset directory would otherwise be
//! a blank page with nothing in the log to say why.

use bytes::Bytes;

use crate::server::State;
use crate::transport::{Reply, text};

pub(crate) fn web(path: &str, state: &State) -> Reply {
    let Some(root) = state.web_root.as_ref() else {
        return text(404, "web UI not installed");
    };
    let rel = match path.trim_start_matches('/') {
        "" => "index.html",
        other => other,
    };
    if rel.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        return text(400, "bad path");
    }
    let full = root.join(rel);
    let Some(bytes) = asset_bytes(root, rel) else {
        return text(404, "not found");
    };
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map" | "json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    // index.html may not be cached: its whole job is to name the current
    // hashes, so a cached copy is exactly how a browser ends up requesting
    // assets that no longer exist. robots.txt is not content-addressed
    // either. Everything else can be kept forever, because the only URLs that
    // reach those files were minted here, with a hash that changes when the
    // bytes do.
    let cache = if matches!(rel, "index.html" | "robots.txt") {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    // The index costs no copy: `served_index` hands back a `&'static str`, so
    // its bytes are static too.
    let body = if rel == "index.html" {
        Bytes::from_static(served_index(root).as_bytes())
    } else {
        Bytes::from(bytes)
    };
    Reply::bytes(200, body)
        .with_header("content-type", ctype.to_owned())
        .with_header("cache-control", cache.to_owned())
        // The UI holds an admin token in memory; keep it out of any embedding
        // page and out of a referrer.
        .with_header("x-content-type-options", "nosniff".to_owned())
        .with_header("x-frame-options", "DENY".to_owned())
        .with_header("referrer-policy", "no-referrer".to_owned())
}

pub(crate) fn distro_asset(rel: &str) -> Option<&'static str> {
    match rel {
        "vendor/bootstrap.min.css" => Some("/usr/share/javascript/bootstrap5/css/bootstrap.min.css"),
        _ => None,
    }
}

const VERSIONED: &[&str] = &[
    "vendor/bootstrap.min.css",
    "vendor/vue.global.prod.js",
    "charts.js",
    "app.js",
];

/// Read an asset the way `web` does: our tree first, the distribution's copy
/// second.
pub(crate) fn asset_bytes(root: &std::path::Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel))
        .ok()
        .or_else(|| distro_asset(rel).and_then(|p| std::fs::read(p).ok()))
}

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) fn integrity(bytes: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha384};
    let digest = Sha384::digest(bytes);
    format!(
        "sha384-{}",
        base64::engine::general_purpose::STANDARD.encode(&digest[..])
    )
}

pub(crate) fn versioned_index(root: &std::path::Path, html: &str) -> String {
    let mut out = html.to_owned();
    for rel in VERSIONED {
        let Some(bytes) = asset_bytes(root, rel) else {
            // Missing asset: leave the reference alone, so the browser's own
            // 404 says so rather than a broken URL hiding which one it was.
            continue;
        };
        out = out.replace(
            &format!("\"{rel}\""),
            &format!(
                "\"{rel}?v={}\" integrity=\"{}\"",
                content_hash(&bytes),
                integrity(&bytes)
            ),
        );
    }
    out
}

static VERSIONED_INDEX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The UI's index, with each asset reference versioned and verified.
pub(crate) fn served_index(root: &std::path::Path) -> &'static str {
    VERSIONED_INDEX.get_or_init(|| {
        // `asset_bytes` has already established this file exists.
        let Ok(bytes) = std::fs::read(root.join("index.html")) else {
            return String::new();
        };
        match String::from_utf8(bytes) {
            Ok(html) => versioned_index(root, &html),
            // Not UTF-8, so not ours to rewrite; serve it as found rather than
            // fail a page load over a rewrite that could not be performed.
            Err(e) => String::from_utf8_lossy(&e.into_bytes()).into_owned(),
        }
    })
}
