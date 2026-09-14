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

/// The CORS headers a browser needs before it will talk to the API.
///
/// Answered here rather than by a fairing so that it is one row of the route
/// table: in the packaged build the UI and the API share an origin and this is
/// never reached, but under `vite dev` the UI is on another port and without it
/// the browser refuses every request with a message about CORS that names
/// neither the server nor the missing header.
#[must_use]
pub(crate) fn preflight() -> Reply {
    let mut reply = Reply::empty(204);
    for (name, value) in [
        ("access-control-allow-origin", "*"),
        ("access-control-allow-methods", "GET,POST,PUT,DELETE,OPTIONS"),
        (
            "access-control-allow-headers",
            "content-type,authorization,x-api-key,anthropic-version,anthropic-beta,\
             x-tab-atelier-token",
        ),
        ("access-control-max-age", "600"),
    ] {
        reply = reply.with_header(name, value.to_owned());
    }
    reply
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A state whose web root is a directory the test controls.
    fn served_from(root: std::path::PathBuf) -> State {
        let mut state = State::for_tests("t".to_owned());
        state.web_root = Some(root);
        state
    }

    /// The repository's own `assets/` directory.
    fn repo_assets() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
    }

    // ── the digests ─────────────────────────────────────────────────────────

    #[test]
    fn an_integrity_digest_is_a_base64_sha384() {
        // The prefix and the length are what a browser checks before it will
        // use the asset at all; the wrong algorithm means every script is
        // refused and the page is blank with one console line.
        use base64::Engine as _;
        let sri = integrity(b"alert(1)");
        let encoded = sri
            .strip_prefix("sha384-")
            .expect("the algorithm prefix a browser requires");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(raw.len(), 48, "sha384 is 48 bytes");
        assert_ne!(sri, integrity(b"alert(2)"), "different bytes must not share a digest");
    }

    #[test]
    fn the_same_bytes_always_produce_the_same_digest() {
        // The digest is computed per request. A digest that varied would make
        // every response advertise a hash the cached copy does not have, and
        // the browser would refetch the asset every time.
        assert_eq!(integrity(b"body{}"), integrity(b"body{}"));
    }

    // ── the shipped page ────────────────────────────────────────────────────

    /// The shipped page against the shipped assets: every reference the browser
    /// will act on is versioned, and each one advertises a digest. A reference
    /// missed here is a stale asset no cache header can save.
    #[test]
    fn the_ui_is_served_with_versioned_and_verified_assets() {
        let root = repo_assets();
        let html = std::fs::read_to_string(root.join("index.html")).expect("the committed UI");
        let out = versioned_index(&root, &html);

        // Only the assets this machine can read. Bootstrap is not vendored —
        // it comes from Debian's libjs-bootstrap5, a runtime dependency of the
        // .deb and absent from a bare CI runner. Asserting on it would be
        // asserting that this developer has the package installed, so the check
        // is over what resolves and the size of that set is asserted below.
        let mut checked = 0;
        for rel in VERSIONED {
            if asset_bytes(&root, rel).is_none() {
                continue;
            }
            checked += 1;
            let marker = format!("\"{rel}?v=");
            let at = out
                .find(&marker)
                .unwrap_or_else(|| panic!("{rel} is not versioned anywhere in the page"));
            let reference = &out[at..out.len().min(at + 128)];
            assert!(
                reference.contains("integrity=\"sha384-"),
                "{rel} is versioned but carries no digest: {reference}"
            );
        }
        assert!(
            checked > 0,
            "no listed asset was readable, so the loop above proved nothing"
        );
    }

    #[test]
    fn a_page_with_nothing_to_version_is_left_alone() {
        // The marker is inserted into the page, so a page without any of the
        // referenced assets must come back byte-for-byte: a stray rewrite would
        // corrupt whatever markup happened to match.
        let root = std::env::temp_dir().join("tab-atelier-web-empty");
        let _ = std::fs::create_dir_all(&root);
        let html = "<html><body><p>nothing to see</p></body></html>";
        assert_eq!(versioned_index(&root, html), html);
    }

    // ── the served root ─────────────────────────────────────────────────────

    #[test]
    fn the_web_root_cannot_be_climbed_out_of() {
        // Every one of these is an attempt to read a file outside the web root
        // by naming it from inside. Refusing them is the difference between a
        // static file server and a way to read `/etc/shadow` over HTTP.
        let state = served_from(std::path::PathBuf::from("/usr/share/tab-atelier-proxy/web"));
        for attack in [
            "/../../../../etc/passwd",
            "/vendor/../../../etc/shadow",
            "/./secret",
            "//etc/passwd",
        ] {
            let response = web(attack, &state);
            assert!(
                response.status == 400 || response.status == 404,
                "{attack} was not refused: {}",
                response.status
            );
        }
    }

    #[test]
    fn an_installation_with_no_web_root_says_so_rather_than_failing_oddly() {
        // A server started without the UI package is a real deployment, and a
        // 404 naming the reason is better than an empty 200.
        let state = State::for_tests("t".to_owned());
        assert!(state.web_root.is_none(), "the default has no UI");
        assert_eq!(web("/", &state).status, 404);
    }

    #[test]
    fn a_request_for_the_root_is_served_the_page() {
        // The one path that has to work: the browser asks for `/`, and the file
        // behind it is `index.html`.
        let state = served_from(repo_assets());
        let response = web("/", &state);
        assert_eq!(response.status, 200, "the committed assets are servable");
    }

    // ── the distribution's libraries ────────────────────────────────────────

    /// Bootstrap comes from the distribution, so neither the repository nor the
    /// package carries a copy — and a source checkout needs no symlink.
    #[test]
    fn bootstrap_is_served_from_the_distribution_package() {
        assert_eq!(
            distro_asset("vendor/bootstrap.min.css"),
            Some("/usr/share/javascript/bootstrap5/css/bootstrap.min.css")
        );
        // Only the libraries the distribution actually packages. Vue is not one
        // of them, so it stays vendored rather than 404ing at runtime.
        assert_eq!(distro_asset("vendor/vue.global.prod.js"), None);
        assert_eq!(distro_asset("app.js"), None);
        assert_eq!(distro_asset("../../../etc/passwd"), None);

        let root = repo_assets();
        assert!(
            !root.join("vendor/bootstrap.min.css").exists(),
            "a local copy would shadow the distribution's and stop getting updates"
        );
        assert!(
            root.join("vendor/vue.global.prod.js").is_file(),
            "Vue has no distribution package, so it has to be in the tree"
        );
    }

    // ── the preflight ───────────────────────────────────────────────────────

    #[test]
    fn a_preflight_allows_the_headers_this_api_reads() {
        // Under `vite dev` the UI is on another port, and the browser refuses
        // every request until it is told each of these is acceptable. A missing
        // one shows up as a CORS message that names neither the header nor the
        // server.
        let reply = preflight();
        assert_eq!(reply.status, 204);
        let allowed = reply
            .headers
            .iter()
            .find(|(n, _)| *n == "access-control-allow-headers")
            .map(|(_, v)| v.clone())
            .expect("the header list");
        for needed in ["authorization", "x-api-key", "anthropic-version", "content-type"] {
            assert!(allowed.contains(needed), "{needed} is not allowed: {allowed}");
        }
    }

    #[test]
    fn a_preflight_carries_no_body() {
        // 204 with a body is a protocol error, and the answer has to be
        // readable before the request it is a preflight for is even sent.
        let reply = preflight();
        assert!(matches!(reply.body, crate::transport::ReplyBody::Bytes(ref b) if b.is_empty()));
    }
}

#[cfg(test)]
mod ui_tests {
    /// The UI's HTML must be well-formed enough for Vue to compile it.
    ///
    /// Vue compiles `index.html`'s body as a template at runtime, so a
    /// malformed comment or an unbalanced tag is not a cosmetic problem: the
    /// compile throws and NOTHING renders. A blank page with one console line
    /// is the worst failure mode in this UI, and it has happened twice — once
    /// from a `->` typo closing a comment, which swallowed the rest of the
    /// template.
    ///
    /// This checks the two structural properties that caused it. It is not an
    /// HTML parser and does not try to be; `@vue/compiler-dom` is the real
    /// authority, but requiring node in the test run to reach it would cost
    /// more than it is worth for the failure it catches.
    #[test]
    fn the_ui_html_is_structurally_sound() {
        let html = include_str!("../../../assets/index.html");

        // Every comment must close. An unclosed one eats everything after it.
        let opens = html.matches("<!--").count();
        let closes = html.matches("-->").count();
        assert_eq!(
            opens, closes,
            "unbalanced HTML comments in index.html ({opens} <!-- vs {closes} -->) — \
             an unclosed comment swallows the rest of the template and Vue renders nothing"
        );

        // And the elements Vue cares about must balance.
        let mut depth: std::collections::BTreeMap<&str, i32> = std::collections::BTreeMap::new();
        for tag in [
            "div", "table", "tbody", "thead", "tr", "td", "th", "form", "template", "p", "span",
        ] {
            let open = html.matches(&format!("<{tag} ")).count() + html.matches(&format!("<{tag}>")).count();
            let close = html.matches(&format!("</{tag}>")).count();
            if open != close {
                depth.insert(
                    tag,
                    i32::try_from(open).unwrap_or(0) - i32::try_from(close).unwrap_or(0),
                );
            }
        }
        assert!(depth.is_empty(), "unbalanced tags in index.html: {depth:?}");

        // Self-closing syntax on a component is the subtle one, and it is
        // invisible to a string-based template compiler.
        //
        // `<my-chart />` is honoured when Vue compiles a template STRING. This
        // file is an in-DOM template: the browser's HTML parser gets it first,
        // and HTML has no self-closing syntax for non-void elements. The slash
        // is ignored, the tag stays open, and every following sibling becomes a
        // CHILD of the component — which shows up as a baffling "v-else has no
        // adjacent v-if" from somewhere far below.
        for (i, line) in html.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('<') {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                // Hyphen means a custom element: a Vue component, not HTML.
                if name.contains('-') {
                    assert!(
                        !line.trim_end().ends_with("/>"),
                        "index.html:{}: <{name} … /> is self-closed. HTML ignores that, so the tag \
                         stays open and the rest of the template becomes its children:\n  {line}",
                        i + 1
                    );
                }
            }
        }
    }
}
