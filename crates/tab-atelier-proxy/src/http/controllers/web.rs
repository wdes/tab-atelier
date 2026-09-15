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
    let ctype = mime_of(full.extension().and_then(|e| e.to_str()));
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
        // The style sheet ends with `sourceMappingURL=bootstrap.min.css.map`, so
        // the browser asks for this the moment the CSS loads. The package ships
        // it; without the entry here the request fell through to our own assets
        // and 404'd — a console error on every page load, from a file we were
        // deliberately not serving.
        "vendor/bootstrap.min.css.map" => Some("/usr/share/javascript/bootstrap5/css/bootstrap.min.css.map"),
        _ => None,
    }
}

const VERSIONED: &[&str] = &[
    "vendor/bootstrap.min.css",
    "vendor/vue.global.prod.js",
    // `api.js` most of all: it names every route the page calls, so a cached
    // copy after an upgrade is a client asking for endpoints that may not
    // exist. `app.js` is committed alongside it and changes at least as often,
    // but a stale one of those fails visibly — a stale `api.js` fails as a
    // request to the wrong path.
    "api.js",
    "charts.js",
    "app.js",
];

/// The content type for a file extension.
///
/// Pulled out of the handler so it can be tested on its own, which is not
/// academic: the `Responder` adapter once overrode every one of these with
/// `application/json`, and a browser told not to sniff then refused to execute
/// the dashboard's own scripts. The types this returns are load-bearing.
#[must_use]
fn mime_of(extension: Option<&str>) -> &'static str {
    match extension {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map" | "json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        // Named rather than left to the `octet-stream` default: browsers sniff a
        // favicon either way, but a wrong type shows up as a broken icon in a
        // tab and as a download in some clients.
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

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
mod mime_tests {
    use super::mime_of;

    /// The extensions a browser will actually fetch, by the type it demands.
    ///
    /// Extensions, not filenames — the function takes what follows the last
    /// dot. An earlier version of this test passed `"app.js"` and every case
    /// fell through to the default, which made the test below assert that
    /// `application/octet-stream != application/json` and prove nothing.
    const EXPECTED: [(&str, &str); 6] = [
        ("html", "text/html; charset=utf-8"),
        ("js", "application/javascript; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
        ("txt", "text/plain; charset=utf-8"),
        ("ico", "image/x-icon"),
        ("json", "application/json"),
    ];

    #[test]
    fn every_extension_the_ui_serves_has_a_real_type() {
        for (ext, want) in EXPECTED {
            assert_eq!(mime_of(Some(ext)), want, "{ext}");
        }
    }

    /// No served asset may be typed as JSON.
    ///
    /// The bug this pins: the `Responder` adapter applied `application/json` to
    /// every buffered body, overriding what this function worked out — so a
    /// `.js` went out as JSON, and a browser told not to sniff refused to
    /// execute it. The page rendered blank with one console line and no clue
    /// which layer was responsible.
    #[test]
    fn no_served_asset_is_typed_as_json() {
        for (ext, _) in EXPECTED {
            // `.json` is the one extension that SHOULD be JSON, and nothing the
            // dashboard loads is one.
            if ext == "json" {
                continue;
            }
            assert_ne!(mime_of(Some(ext)), "application/json", "{ext} is typed as JSON");
        }
    }

    #[test]
    fn an_unknown_extension_falls_back_to_octets_rather_than_to_json() {
        // `application/octet-stream` makes a client download the file; JSON
        // would make a browser try to parse it. Neither is right for an unknown
        // type, and octets is the one that cannot be mistaken for a statement
        // about the contents.
        assert_eq!(mime_of(Some("wasm")), "application/octet-stream");
        assert_eq!(mime_of(None), "application/octet-stream");
        assert_eq!(mime_of(Some("")), "application/octet-stream");
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

        // No bare `<template>` anywhere.
        //
        // This is the root template — `mount("#app")` compiles what is inside
        // `#app` — and at the root a bare template renders as an EMPTY
        // `<template>` element with its children dropped. The app mounts, every
        // request succeeds, and the page is blank. It reached a deployed host
        // exactly once, and the rendered DOM was
        // `<div id="app" data-v-app=""><template></template></div>`.
        //
        // `<template v-if>` and `<template v-else>` are fine and are the reason
        // this checks for the tag alone rather than the tag at all.
        for (n, line) in html.lines().enumerate() {
            assert!(
                !line.trim_start().starts_with("<template>"),
                "index.html:{}: a bare <template> at the root renders as an empty element and \
                 drops everything inside it. Remove the wrapper — the root's children are \
                 rendered as a fragment — or give it a directive:\n  {line}",
                n + 1
            );
        }

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

    /// The chart templates are checked the same way `index.html` is.
    ///
    /// `charts.js` holds its markup in Vue template strings, and Vue compiles
    /// them at runtime — so they fail exactly the way `index.html` does: an
    /// unclosed comment or an unbalanced tag throws during the compile and the
    /// chart renders as nothing, with one console line. The guard above covers
    /// `index.html` and this file was not covered at all, which is the gap this
    /// closes.
    ///
    /// The templates stay strings rather than moving into an HTML `<template>`.
    /// They carry `v-for`, bound attributes and `{{ }}`, so cloning one would
    /// copy those as inert text and the charts would draw once and never
    /// update — and a `<template>` inside `#app` would be compiled by the root
    /// instance and injected as literal DOM. What was worth having from that
    /// idea is the checking, which is this.
    #[test]
    fn every_chart_template_is_structurally_sound() {
        let source = include_str!("../../../assets/charts.js");
        let templates = template_literals(source);
        assert!(
            templates.len() >= 3,
            "expected the three chart templates, found {} — has the markup moved?",
            templates.len()
        );

        for (n, template) in templates.iter().enumerate() {
            let opens = template.matches("<!--").count();
            let closes = template.matches("-->").count();
            assert_eq!(
                opens, closes,
                "chart template {n}: unbalanced HTML comments ({opens} <!-- vs {closes} -->) — \
                 an unclosed one swallows the rest of the template and the chart renders nothing"
            );

            // SVG in a template *string* is compiled by `@vue/compiler-dom`,
            // which honours self-closing syntax — unlike the browser's HTML
            // parser, which is why `index.html` forbids it and this does not.
            // What must balance here is the container elements.
            for tag in ["div", "svg", "g", "span", "template", "text"] {
                let open =
                    template.matches(&format!("<{tag} ")).count() + template.matches(&format!("<{tag}>")).count();
                let close = template.matches(&format!("</{tag}>")).count();
                assert_eq!(
                    open, close,
                    "chart template {n}: <{tag}> is unbalanced ({open} open vs {close} close)"
                );
            }
        }
    }

    /// Pull the body of every template literal out of a source file.
    ///
    /// Deliberately naive: it splits on the `template:` key and reads to the
    /// next unescaped backtick. That is enough for this file, where the
    /// templates are the only literals holding markup, and a real parser would
    /// be a dependency to keep working for a check that only needs to see the
    /// text.
    fn template_literals(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find("template: `") {
            rest = &rest[at + "template: `".len()..];
            let Some(end) = rest.find('`') else { break };
            found.push(rest[..end].to_owned());
            rest = &rest[end..];
        }
        found
    }
}

#[cfg(test)]
mod packaging_tests {
    use std::path::{Path, PathBuf};

    use super::distro_asset;

    /// The repository's own `assets/`, which is what the package installs.
    fn repo_assets() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
    }

    /// The crate manifest, which is where the `.deb` asset list lives.
    fn manifest() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).expect("the manifest")
    }

    /// Every path `index.html` pulls in from this origin.
    ///
    /// `<script src=…>` and `<link … href=…>` are the two ways this page reaches
    /// for another file. Anything with a scheme, or a bare `#`, is skipped: a
    /// URL to somewhere else is not ours to ship, and an anchor is not a file.
    fn referenced(html: &str) -> Vec<String> {
        let mut found = Vec::new();
        for key in ["src=\"", "href=\""] {
            for (at, _) in html.match_indices(key) {
                let rest = &html[at + key.len()..];
                let Some((path, _)) = rest.split_once('"') else {
                    continue;
                };
                if !path.starts_with('#') && !path.contains("://") {
                    found.push(path.to_owned());
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    /// The `assets/…` paths the `.deb` is told to install.
    ///
    /// Read line by line to the closing bracket, rather than by taking
    /// everything after `assets = [` up to the next `]`: each entry is itself a
    /// bracketed array, so the first `]` belongs to the first entry. Splitting
    /// there sees one asset and silently ignores the rest, which is a check that
    /// passes while testing almost nothing.
    fn packaged(manifest: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut inside = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if !inside {
                inside = trimmed.starts_with("assets = [");
                continue;
            }
            if trimmed == "]" {
                break;
            }
            if let Some(at) = trimmed.find("\"assets/") {
                let rest = &trimmed[at + 1..];
                if let Some((path, _)) = rest.split_once('"') {
                    found.push(path.to_owned());
                }
            }
        }
        assert!(
            inside,
            "no `assets = [` in Cargo.toml — has the packaging layout changed?"
        );
        found
    }

    /// Everything the page references is a file the package will actually have.
    ///
    /// The failure this pins, which reached a deployed host: `index.html` gained
    /// `<script src="api.js">` and the `.deb` asset list did not, so the package
    /// installed a page whose HTTP client was absent. Every page load 404'd on
    /// it and the dashboard was blank — a total failure of the only UI there is,
    /// from a one-line omission that nothing checked.
    ///
    /// A reference is satisfied either by our own tree, in which case the
    /// package must list it, or by `distro_asset`, which resolves to the
    /// distribution's copy. Asking that function rather than repeating its table
    /// is deliberate: a mapping added there is covered here with nobody having
    /// to remember this test exists.
    #[test]
    fn every_asset_the_page_references_is_one_the_package_ships() {
        let root = repo_assets();
        let html = std::fs::read_to_string(root.join("index.html")).expect("the committed UI");
        let manifest = manifest();
        let packaged = packaged(&manifest);
        let refs = referenced(&html);
        assert!(
            refs.len() >= 4,
            "only {} references found in index.html — the scan has stopped working, so this \
             test would pass for the wrong reason: {refs:?}",
            refs.len()
        );

        for rel in refs {
            if let Some(from_distro) = distro_asset(&rel) {
                assert!(
                    from_distro.starts_with('/'),
                    "{rel} resolves to {from_distro:?}, which is not an absolute path"
                );
                continue;
            }
            assert!(
                root.join(&rel).exists(),
                "index.html references {rel}, which is not in assets/ and not served by the \
                 distribution — the page will 404 on it"
            );
            assert!(
                packaged.iter().any(|p| p == &format!("assets/{rel}")),
                "index.html references {rel} and assets/{rel} exists, but the .deb asset list \
                 in Cargo.toml does not include it — the package would install a page whose \
                 {rel} is missing. Add:\n    \
                 [\"assets/{rel}\", \"/usr/share/tab-atelier-proxy/web/\", \"644\"],"
            );
        }
    }

    /// And the other direction: nothing shipped is left unreferenced by accident.
    ///
    /// Weaker than the check above and kept deliberately narrow — it only asks
    /// that every path the package installs is a file that exists, so a rename
    /// in one place cannot leave a `.deb` build failing at package time with a
    /// message about a missing source file.
    #[test]
    fn nothing_the_package_installs_is_missing_from_the_tree() {
        let root = repo_assets();
        let manifest = manifest();

        for path in packaged(&manifest) {
            let rel = path.strip_prefix("assets/").expect("all entries are under assets/");
            assert!(root.join(rel).exists(), "Cargo.toml ships {path}, which does not exist");
        }
    }

    /// A style sheet we serve from the distribution brings its own source map.
    ///
    /// Bootstrap's CSS ends with `sourceMappingURL=bootstrap.min.css.map`, so
    /// the browser asks for that the moment the style sheet loads. The package
    /// ships it, but our resolver only knew about the `.css` — so every page
    /// load produced a 404 for a file we were deliberately not serving, which
    /// reads as a bug in this proxy and is a missing entry in one `match`.
    ///
    /// Skipped when the distribution's file is absent, so a machine without
    /// `libjs-bootstrap5` — a bare CI runner — is not failing on a package it
    /// does not have.
    #[test]
    fn a_distribution_style_sheet_brings_its_source_map_with_it() {
        for css in ["vendor/bootstrap.min.css"] {
            let Some(real) = distro_asset(css) else {
                panic!("{css} is served from nowhere");
            };
            let Ok(body) = std::fs::read_to_string(real) else {
                eprintln!("{real} is not installed here — skipping");
                continue;
            };
            if !body.contains("sourceMappingURL=") {
                continue;
            }
            let map = format!("{css}.map");
            assert!(
                distro_asset(&map).is_some(),
                "{css} declares a source map and the browser will request {map}, which this \
                 proxy does not resolve — it will 404 on every page load. Add it to \
                 `distro_asset`."
            );
        }
    }

    #[test]
    fn the_asset_scans_are_not_vacuous() {
        // Both parsers are textual, so a change to the markup or the manifest
        // would silently make them find nothing — and a check over an empty list
        // passes. This pins that they still see what is there.
        assert_eq!(
            packaged(
                "assets = [\n    [\"assets/a\", \"/y\", \"644\"],\n    \
                 [\"assets/b\", \"/y\", \"644\"],\n]"
            ),
            vec!["assets/a".to_owned(), "assets/b".to_owned()],
            "both entries, not just the first"
        );
        assert_eq!(
            referenced("<script src=\"a.js\"></script><a href=\"#\">x</a><a href=\"https://e/x\">y</a>"),
            vec!["a.js".to_owned()],
            "scheme URLs and anchors are not files to ship"
        );
    }
}
