// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `/tabs/<id>/view` share-link viewer: the xterm.js document with the
//! tab's name/background/key templated in, served with no-store + strict CSP.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{
    BUILD_HASH, TabSnapshot, VIEWER_HTML, error_json, is_safe_hex_color, parse_tab_key, resolve_tab_idx,
    respond_with_etag,
};

pub(super) fn run<W: Write>(
    stream: &mut W,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/view") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state_g, key_raw, is_uuid) else {
        drop(state_g);
        error_json(stream, 404, "tab not found");
        return;
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
    // Relative hop from the viewer document back to the mount
    // root so `<prefix>/assets/...` references resolve under any
    // reverse-proxy prefix (the proxy strips the prefix before
    // the request reaches us, so absolute `/assets/...` URLs
    // bypass it and 404). The document lives at
    // `<prefix>/tabs/{key}/view`; its directory is
    // `<prefix>/tabs/{key}/`, so one `../` per path segment in
    // `tabs/{key}` climbs back to `<prefix>/`:
    //   - `/tabs/0/view`            → `../../`
    //   - `/tabs/by-id/<uuid>/view` → `../../../`
    let asset_depth = 1 + key_for_html.split('/').filter(|s| !s.is_empty()).count();
    let asset_prefix = "../".repeat(asset_depth);
    // The tab name lands in two distinct contexts: inside
    // <title> (HTML-escape) and inside a JS string literal
    // (JSON-encode — handles quotes, backslashes, newlines,
    // and any future weirdness in one go). Using two
    // substitution markers keeps each context safe.
    let html_name = tab_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    // serde_json::to_string yields a quoted JS-safe string
    // literal; strip the surrounding quotes so the template
    // can wrap it in its own quotes.
    //
    // serde_json escapes quotes/backslashes/control chars but
    // NOT `<`, `>`, or `&` — and the HTML parser ends the
    // inline <script> element on the literal byte sequence
    // `</script>` regardless of JS string context. Since the
    // viewer's CSP allows 'unsafe-inline', an unescaped
    // `</script><script>…` tab name would break out and run.
    // Re-escape those three as JS `\uXXXX` so the value stays a
    // valid string literal that can never terminate the script
    // element. (`__TAB_NAME_HTML__` above is separately escaped
    // for its <title> context.)
    let js_name = serde_json::to_string(&tab_name)
        .unwrap_or_else(|_| "\"\"".into())
        .trim_matches('"')
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    // Validate that bg_color looks like #RRGGBB before
    // inlining into HTML / CSS (defense against a malformed
    // value in tabs.json or someone POSTing junk into the
    // bg-color endpoint). Fall back to the default on
    // anything sketchy.
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
    // Tell browsers (and any intervening CDN) not to cache
    // the viewer HTML — we ship JS fixes in the deb and
    // users would otherwise see a stale banner / poll loop
    // until a hard reload.
    respond_with_etag(
        stream,
        200,
        "text/html; charset=utf-8",
        html.as_bytes(),
        accept_gzip,
        if_none_match,
        // Cache headers + clickjacking guards. CSP locks the
        // page to its own origin for everything (no inline
        // scripts despite the template subs — they live in a
        // pinned `<script>` set up to read `window.TAB`, no
        // user-controlled JS). X-Frame-Options blocks iframe
        // embedding of share links into phishing pages.
        "Cache-Control: no-store, no-cache, must-revalidate\r\n\
         Pragma: no-cache\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
         Referrer-Policy: no-referrer\r\n",
    );
}
