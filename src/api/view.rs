// SPDX-License-Identifier: MPL-2.0

//! The `/tabs/<id>/view` share-link viewer: the xterm.js document with the
//! tab's name/background/key templated in, served with no-store + strict CSP.

use std::sync::{Arc, Mutex};

use tab_atelier_api::{Failure, Reply, Request, cached};

use super::{BUILD_HASH, TabSnapshot, VIEWER_HTML, is_safe_hex_color, parse_tab_key, resolve_tab_idx};

/// How many `../` a `/view` document needs to climb back to the API root.
///
/// The document lives at `<prefix>/tabs/{key}/view`, whose directory is
/// `<prefix>/tabs/{key}/` — one `../` per segment of `tabs/{key}`, plus one.
///
/// `trailing_slash` matters because a proxy that rewrites `/view` to `/view/`
/// (Cloudflare Tunnel does) moves the document one directory deeper without
/// changing the route we matched. Ignoring it served the page and 404'd every
/// stylesheet and script it referenced.
#[must_use]
const fn asset_depth_for(segments: usize, trailing_slash: bool) -> usize {
    1 + segments + if trailing_slash { 1 } else { 0 }
}

#[must_use]
fn asset_depth(key: &str, trailing_slash: bool) -> usize {
    asset_depth_for(key.split('/').filter(|s| !s.is_empty()).count(), trailing_slash)
}

/// The body of a JS string literal holding `name` — everything between the
/// quotes, safe to paste into `const NAME = "…";`.
///
/// JSON-encodes (quotes, backslashes, newlines, control characters), then
/// strips exactly the one quote JSON puts at each end, then neutralises
/// `<`/`>`/`&` so the value cannot close the surrounding `<script>` element.
///
/// The "exactly one" matters. This used to `trim_matches('"')`, which strips
/// *every* quote at each end: a tab named `foo"` encodes as `"foo\""`, whose
/// final two characters are both quotes, so trimming left `foo\` — and that
/// trailing backslash escaped the literal's closing quote, producing invalid
/// JavaScript and a blank viewer for that tab.
#[must_use]
fn js_string_body(name: &str) -> String {
    let encoded = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
    encoded
        .get(1..encoded.len().saturating_sub(1))
        .unwrap_or("")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

pub(super) fn run(
    request: &Request<'_>,
    state: &Arc<Mutex<TabSnapshot>>,
    // The request path ended in `/`, so the document sits one directory
    // deeper than the route match suggests.
    trailing_slash: bool,
) -> Result<Reply, Failure> {
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/view") else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state_g, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let t = &state_g.tabs[idx];
    let tab_name = t.name.clone();
    let tab_bg = if t.bg_color.is_empty() {
        crate::DEFAULT_TAB_BG_COLOR.to_string()
    } else {
        t.bg_color.to_string()
    };
    drop(state_g);
    let key_for_html = if is_uuid {
        format!("by-id/{key_raw}")
    } else {
        key_raw.to_string()
    };
    // Relative hop from the viewer document back to the mount root so
    // `<prefix>/assets/...` references resolve under any reverse-proxy prefix
    // (the proxy strips the prefix before the request reaches us, so absolute
    // `/assets/...` URLs bypass it and 404). The document lives at
    // `<prefix>/tabs/{key}/view`; its directory is `<prefix>/tabs/{key}/`, so
    // one `../` per path segment in `tabs/{key}` climbs back to `<prefix>/`:
    //   - `/tabs/0/view`            → `../../`
    //   - `/tabs/by-id/<uuid>/view` → `../../../`
    let asset_depth = asset_depth(&key_for_html, trailing_slash);
    let asset_prefix = "../".repeat(asset_depth);
    // The tab name lands in two distinct contexts: inside <title> (HTML-escape)
    // and inside a JS string literal (JSON-encode — handles quotes,
    // backslashes, newlines, and any future weirdness in one go). Using two
    // substitution markers keeps each context safe.
    let html_name = tab_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let js_name = js_string_body(&tab_name);
    // Validate that bg_color looks like #RRGGBB before inlining into HTML / CSS
    // (defense against a malformed value in tabs.json or someone POSTing junk
    // into the bg-color endpoint). Fall back to the default on anything
    // sketchy.
    let safe_bg: &str = if is_safe_hex_color(&tab_bg) {
        &tab_bg
    } else {
        crate::DEFAULT_TAB_BG_COLOR
    };
    let html = VIEWER_HTML
        .replace("__ASSET_PREFIX__", &asset_prefix)
        .replace("__TAB_KEY__", &key_for_html)
        .replace("__TAB_NAME_HTML__", &html_name)
        .replace("__TAB_NAME_JS__", &js_name)
        .replace("__TAB_BG__", safe_bg)
        .replace("__BUILD_HASH__", BUILD_HASH);
    // Tell browsers (and any intervening CDN) not to cache the viewer HTML — we
    // ship JS fixes in the deb and users would otherwise see a stale banner /
    // poll loop until a hard reload.
    //
    // `cache` gives the conditional handling and the `Vary`; the four headers
    // below are the part only this document wants, and they are stated here
    // rather than threaded through the shared helper because nothing else in
    // the API needs a CSP this tight.
    let etag = format!("{:08x}", crate::crc32(html.as_bytes()));
    let reply = cached(
        200,
        "text/html; charset=utf-8",
        bytes::Bytes::from(html),
        &etag,
        request.conditional,
    );
    Ok(reply
        .with_header("cache-control", "no-store, no-cache, must-revalidate".to_owned())
        .with_header("pragma", "no-cache".to_owned())
        // Clickjacking guard: share links must not be iframe-able into a
        // phishing page.
        .with_header("x-frame-options", "DENY".to_owned())
        // CSP locks the page to its own origin for everything. `'unsafe-inline'`
        // is required because the template substitutes into inline script; the
        // JS-string escaping above is what makes that safe (an unescaped
        // `</script>` in a tab name would otherwise break out).
        .with_header(
            "content-security-policy",
            "default-src 'none'; script-src 'self' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
             connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
                .to_owned(),
        )
        .with_header("referrer-policy", "no-referrer".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{asset_depth, js_string_body};

    #[test]
    fn a_proxy_added_trailing_slash_still_resolves_the_assets() {
        // `/tabs/0/view` — document dir `/tabs/0/`, so `../../` reaches the
        // API root.
        assert_eq!(asset_depth("0", false), 2);
        // `/tabs/by-id/<uuid>/view` is one segment deeper.
        assert_eq!(asset_depth("by-id/abc", false), 3);
        // With the slash the browser resolves against `/tabs/0/view/`, which
        // is one directory deeper — the route still matched, but every asset
        // URL was short by one `../` and 404'd. Only reproducible behind a
        // proxy that normalises trailing slashes, which is why it survived.
        assert_eq!(asset_depth("0", true), 3);
        assert_eq!(asset_depth("by-id/abc", true), 4);
        // Empty segments (`//`) must not inflate the climb.
        assert_eq!(asset_depth("/0/", false), 2);
    }

    /// Rebuild the literal the template emits, and check it parses back.
    fn as_literal(name: &str) -> String {
        format!("\"{}\"", js_string_body(name))
    }

    #[test]
    fn a_name_ending_in_a_quote_does_not_break_the_literal() {
        // The bug an audit found: `trim_matches('"')` stripped the escaped
        // quote as well as the delimiter, leaving a trailing backslash that
        // escaped the literal's closing quote — invalid JS, blank viewer.
        let lit = as_literal("foo\"");
        assert_eq!(lit, r#""foo\"""#, "{lit}");
        assert_eq!(serde_json::from_str::<String>(&lit).unwrap(), "foo\"");
        // A name that is nothing but quotes is the same bug, harder.
        for name in ["\"", "\"\"", "a\"\"", "\"lead"] {
            let lit = as_literal(name);
            assert_eq!(
                serde_json::from_str::<String>(&lit).unwrap(),
                name,
                "round trip failed for {name:?} -> {lit}"
            );
        }
    }

    #[test]
    fn ordinary_names_survive_unchanged() {
        assert_eq!(js_string_body("build-box"), "build-box");
        assert_eq!(js_string_body(""), "");
        // Backslashes and newlines are JSON's job, and must stay escaped.
        assert_eq!(js_string_body("a\\b"), "a\\\\b");
        assert_eq!(js_string_body("a\nb"), "a\\nb");
        assert_eq!(serde_json::from_str::<String>(&as_literal("a\nb")).unwrap(), "a\nb");
    }

    #[test]
    fn angle_brackets_cannot_close_the_script_element() {
        // A tab named `</script>` must not end the inline script — that is an
        // XSS vector, not a cosmetic issue.
        let body = js_string_body("</script><img src=x onerror=alert(1)>");
        assert!(!body.contains('<'), "{body}");
        assert!(!body.contains('>'), "{body}");
        assert!(body.contains("\\u003c/script\\u003e"), "{body}");
        // & is escaped too, so an entity can't be reassembled by the parser.
        assert!(!js_string_body("a&b").contains('&'));
        // And it still decodes to the original text for display.
        assert_eq!(
            serde_json::from_str::<String>(&as_literal("</script>")).unwrap(),
            "</script>"
        );
    }
}
