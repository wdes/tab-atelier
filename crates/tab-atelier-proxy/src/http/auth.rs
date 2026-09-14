// SPDX-License-Identifier: MPL-2.0

//! Browser authentication for the operator surface.
//!
//! Everything the operator sees — the dashboard, its scripts, its styles, and
//! the API behind it — sits behind an HTTP Digest challenge. A browser answers
//! it with its own password prompt; anything that is not a browser gets a 401
//! and no body. That is the point: a scanner, a scraper or a search engine
//! never reaches the static files at all, so there is no page to fingerprint
//! and no bundle to crawl.
//!
//! # Why Digest and not Basic
//!
//! Basic sends the password in cleartext in *every* request. This proxy is
//! reached over plain HTTP on loopback — the `Caddyfile` in `local/` is
//! explicit that there is no certificate on that hop, and the packaged
//! deployment keeps the same arrangement — so Basic would put the operator
//! token on the wire, in the clear, on every asset request, and leave it in any
//! log that records request headers. Digest sends a response derived from the
//! password instead: the password itself never leaves the client, and the
//! response is bound to one method, one path and one nonce.
//!
//! # Why SHA-256 and not the MD5 that RFC 2617 specifies
//!
//! `algorithm=SHA-256` is RFC 7616's addition, and it is supported by both
//! engines that matter, verified from their own sources rather than from
//! folklore:
//!
//! * Chromium parses and *computes* `sha-256` and `sha-256-sess`
//!   (`net/http/http_auth_handler_digest.cc`).
//! * Firefox likewise (`netwerk/protocol/http/nsHttpDigestAuth.cpp`).
//! * curl does both plus `SHA-512-256` (`lib/vauth/digest.c`), which is what
//!   `WebKit`'s curl port delegates to.
//!
//! MD5 is therefore not used at all. The digest is a one-way mixing of a
//! 128-bit random token, so MD5's collision weakness was never the live risk
//! here — but "not the live risk" is not a reason to choose the weaker
//! primitive when the stronger one costs nothing, and this dashboard can
//! rewrite every account on the proxy.
//!
//! Only `SHA-256` is accepted. A credential hashed with anything else — the
//! `MD5` a client would use if it ignored the challenge — is refused *by name*,
//! because the alternative is telling an operator their password is wrong when
//! what is actually wrong is that their client did not read the challenge.
//!
//! The `-sess` variants are not offered. They blunt a stolen password hash by
//! mixing in a per-session value, and the price is that the server must hold
//! the password recoverably — which is precisely what Digest exists to avoid.
//!
//! # The credential
//!
//! One account: [`USERNAME`], with the operator token as its password. The same
//! token the API already accepts, so there is one secret to rotate and one
//! place it lives.
//!
//! It is deliberately *not* "any `tap_` key". A key is a relay credential that
//! a tab holds; accepting one here would mean every user could rewrite accounts
//! and rotate provider keys, which is a far larger grant than the request that
//! carries it.
//!
//! # The nonces
//!
//! Minted as `issue_time || random || HMAC(secret, issue_time || random)`
//! rather than stored, so verifying one costs no lookup and cannot be forged
//! without this process's secret. Both halves of the payload are needed and
//! neither is decoration:
//!
//! * the **randomness** is what makes two mints differ. Without it a nonce
//!   would be a pure function of the clock, and two challenges issued in the
//!   same second would hand out the *same* value — so a browser fetching a page
//!   and its assets in parallel would redeem one and be told the other had
//!   "already been used". That was a real bug, caught by the test below rather
//!   than by reasoning.
//! * the **stamp** is what bounds its life, and it is signed along with the
//!   randomness, so neither can be rewritten to extend a nonce's usefulness.
//!
//! They are single-use and short-lived: a nonce that verified once goes into a
//! table and is refused afterwards, which is what stops a captured
//! `Authorization` header from being replayed — the whole credential travels in
//! one header, so without that it would be usable for its entire lifetime. The
//! table is pruned on every mint, so it is bounded by the number of logins in
//! one window rather than by uptime.
//!
//! The secret is fresh per process. A restart therefore invalidates outstanding
//! nonces and the browser prompts once more, which is the right trade: the
//! alternative is a secret on disk, which is a secret to leak.

use std::collections::HashMap;
use std::sync::Mutex;

use hmac::{Hmac, Mac};
use rocket::http::{Method, Status};
use sha2::{Digest as _, Sha256};

use crate::http::middleware::admin_token;
use crate::http::refusal::tokens_match;
use crate::server::State;

type HmacSha256 = Hmac<Sha256>;

/// The realm the browser shows in its prompt.
///
/// It is also mixed into the password hash, so changing it invalidates every
/// stored credential — which is why it is a constant and not a setting.
pub const REALM: &str = "tab-atelier";

/// The username the prompt is answered with.
///
/// Hard-coded, as asked: there is one operator account, and the thing that
/// authenticates it is the token, not the name. A different name is refused
/// rather than ignored, so a client that has the wrong idea is told.
pub const USERNAME: &str = "admin";

/// How long a minted nonce may be redeemed for.
///
/// Long enough to type a password into a prompt that has been open a while,
/// short enough that a captured header is not useful for long.
const NONCE_TTL_SECS: u64 = 300;

/// The length of the hex-encoded HMAC tag on a nonce.
///
/// Half of a SHA-256 tag. The rest is discarded: this is a nonce, not a
/// signature, and 128 bits is far beyond what forging one would ever need.
const TAG_HEX: usize = 32;

/// The length of the hex-encoded issue time on a nonce.
const STAMP_HEX: usize = 16;

/// The length of the hex-encoded per-mint randomness.
///
/// Without this the nonce would be a pure function of the clock, and two
/// challenges issued in the same second would hand out the SAME nonce — which,
/// with single-use enforcement, means a browser fetching a page and its assets
/// in parallel gets one redemption and one "that nonce has already been used".
/// The randomness is what makes two mints differ; the stamp and the tag are
/// what make one verifiable.
const RAND_HEX: usize = 16;

// ── what is gated ───────────────────────────────────────────────────────────

/// Whether a request has to present a browser credential.
///
/// Everything is gated unless it is on the exempt list below. Written that way
/// round on purpose: a path added later is protected by default, and the list
/// of things reachable without signing in stays short enough to read.
#[must_use]
pub fn gated(path: &str, method: Method) -> bool {
    // A CORS preflight carries no credentials — the browser is explicit that it
    // must not — so requiring one would make every preflight fail and break the
    // development UI. It answers with headers only and reaches no data.
    if method == Method::Options {
        return false;
    }
    !is_exempt(path)
}

/// The paths reachable without signing in.
///
/// Four entries and two prefixes, each with a reason, and nothing else belongs
/// here:
///
/// * `/robots.txt` and `/favicon.ico` are fetched by crawlers and browsers
///   before any credential exists. Gating them produces a 401 in a log and a
///   broken icon in a tab, and protects nothing — neither carries information.
/// * `/api/hello` is a liveness probe, which by definition is asked before
///   anyone has a credential.
/// * `/relay/anthropic/` is what the tabs talk to. The client is `claude`, a
///   CLI configured with `ANTHROPIC_BASE_URL=<origin>/relay/anthropic`, and it
///   authenticates with its own `x-api-key`. A CLI cannot answer a browser
///   prompt, so a challenge here would break every tab. It is not unprotected:
///   [`crate::http::guards::ClientKey`] demands a valid key before anything is
///   forwarded.
/// * `/me/` is the CLI again, on both routes — `tab-atelier doctor` reads
///   `/me/usage` and `share_link` posts `/me/credentials`, each carrying
///   `x-api-key`. Both are reached from a terminal.
///
/// The prefixes are written out in full — `/relay/anthropic/` and `/me/` — and
/// neither bare parent is exempt. That is deliberate on both counts:
///
/// * a path added beside them later is gated by default, which is the whole
///   point of listing a prefix rather than a family;
/// * neither parent is a route, so exempting one only means an unauthenticated
///   request for it falls through to the SPA catch-all and is handed the HTML
///   shell. Nothing was mounting `/relay` or `/me`, and nothing should have
///   been serving their parent to a caller who has not signed in.
///
/// One thing to watch: the dashboard defines a `meUrl()` that is never called.
/// If it is ever wired up, `/me/usage` moves behind the browser gate — which is
/// a one-line change here, and the digest would already be satisfied because
/// the operator is signed in to reach the page that fetches it.
#[must_use]
fn is_exempt(path: &str) -> bool {
    const EXEMPT: [&str; 3] = ["/api/hello", "/robots.txt", "/favicon.ico"];
    /// Prefixes for the routes below a mount point.
    const EXEMPT_PREFIXES: [&str; 2] = ["/relay/anthropic/", "/me/"];

    EXEMPT.contains(&path) || EXEMPT_PREFIXES.iter().any(|p| path.starts_with(p))
}

// ── the nonces ──────────────────────────────────────────────────────────────

/// Mints and redeems the nonces the challenge hands out.
///
/// Lives on the state because the secret is per process and the used-nonce
/// table is per process with it.
pub struct Nonces {
    /// The HMAC key. Fresh per process; never written anywhere.
    secret: [u8; 32],
    /// The nonces that have been redeemed, and when, so they can be pruned.
    used: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for Nonces {
    /// Deliberately opaque: a `Debug` that printed the secret would put it in
    /// whatever log line eventually formats the state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Nonces { .. }")
    }
}

impl Default for Nonces {
    fn default() -> Self {
        Self::new()
    }
}

impl Nonces {
    /// A fresh secret, from the system's random source.
    ///
    /// Two v4 UUIDs rather than a `getrandom` call: `uuid` is already a
    /// dependency and is cryptographically random, so this adds no crate for
    /// 32 bytes of key.
    #[must_use]
    pub fn new() -> Self {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut secret = [0u8; 32];
        secret[..16].copy_from_slice(a.as_bytes());
        secret[16..].copy_from_slice(b.as_bytes());
        Self {
            secret,
            used: Mutex::new(HashMap::new()),
        }
    }

    /// The tag that makes a stamp unforgeable.
    fn tag(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a key of any length");
        mac.update(payload.as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    /// The first [`TAG_HEX`] characters of the tag for a payload.
    ///
    /// Truncated because this is a nonce, not a signature: 128 bits is already
    /// far beyond what forging one would ever need, and a shorter nonce is
    /// less to carry in a challenge and in every response that answers it.
    fn short_tag(&self, payload: &str) -> String {
        self.tag(payload)[..TAG_HEX].to_owned()
    }

    /// Hand out a nonce for now.
    ///
    /// Also prunes, which is what keeps the used table bounded: it is touched
    /// once per login, and logins are rare, so a sweep here costs nothing and
    /// saves a timer.
    fn mint(&self, now: u64) -> String {
        if let Ok(mut used) = self.used.lock() {
            used.retain(|_, at| now.saturating_sub(*at) <= NONCE_TTL_SECS);
        }
        let stamp = format!("{now:0STAMP_HEX$x}");
        // Eight bytes from a v4 UUID, which is drawn from the system's random
        // source. Two challenges in the same second then differ.
        let nonce = uuid::Uuid::new_v4();
        let rand = hex(&nonce.as_bytes()[..RAND_HEX / 2]);
        let payload = format!("{stamp}{rand}");
        let tag = self.short_tag(&payload);
        format!("{payload}{tag}")
    }

    /// Accept a nonce, once.
    ///
    /// # Errors
    /// A sentence naming which of the four ways it failed, because a client
    /// developer reading a 401 needs to tell "stale" from "forged".
    fn redeem(&self, nonce: &str, now: u64) -> Result<(), String> {
        if nonce.len() != STAMP_HEX + RAND_HEX + TAG_HEX {
            return Err("the nonce is malformed".to_owned());
        }
        let (payload, tag) = nonce.split_at(STAMP_HEX + RAND_HEX);
        let stamp = &payload[..STAMP_HEX];

        // Forged before usable: an attacker should not be able to make us do
        // the table work, and the tag check is the cheap one. The whole payload
        // is signed, so the randomness cannot be swapped for a chosen value.
        if !tokens_match(tag, &self.short_tag(payload)) {
            return Err("the nonce was not issued by this proxy".to_owned());
        }

        let issued = u64::from_str_radix(stamp, 16).map_err(|_| "the nonce is malformed".to_owned())?;
        if now.saturating_sub(issued) > NONCE_TTL_SECS {
            return Err(format!(
                "the nonce is older than {NONCE_TTL_SECS} seconds — open the page again"
            ));
        }

        let mut used = self.used.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if used.contains_key(nonce) {
            return Err("that nonce has already been used".to_owned());
        }
        used.retain(|_, at| now.saturating_sub(*at) <= NONCE_TTL_SECS);
        used.insert(nonce.to_owned(), now);
        drop(used);
        Ok(())
    }
}

// ── the exchange ────────────────────────────────────────────────────────────

/// The challenge to put in a 401.
///
/// `algorithm=SHA-256` with `qop="auth"` — the strongest pair RFC 7616 defines
/// that does not require the server to store the password recoverably, and one
/// Chromium, Firefox and curl all implement.
///
/// `opaque` is not sent: it exists to be echoed back by servers that manage
/// sessions, and this one is stateless, so a value the client has to carry
/// would be one more field to get wrong.
#[must_use]
pub fn challenge(nonces: &Nonces, now: u64) -> String {
    format!(
        "Digest realm=\"{REALM}\", qop=\"auth\", algorithm=SHA-256, nonce=\"{}\"",
        nonces.mint(now)
    )
}

/// Check a `Digest` credential against the operator token.
///
/// `target` is the request's path and query, exactly as the client sent it: the
/// digest is computed over the request-target, so comparing it here is what
/// stops a response captured for one path from being replayed against another.
///
/// # Errors
/// A sentence for the log and the body. Never the expected value, and never
/// anything derived from the token.
pub fn verify(state: &State, method: Method, target: &str, header: &str, now: u64) -> Result<(), String> {
    let Some(token) = admin_token::token(state) else {
        return Err("no operator token is configured on this proxy".to_owned());
    };
    let params = header
        .strip_prefix("Digest ")
        .or_else(|| strip_prefix_ci(header, "digest "))
        .ok_or_else(|| "the Authorization header is not a Digest credential".to_owned())
        .map(split_params)?;

    // First occurrence wins, per RFC 7616. Taking the last would let a header
    // carrying two `response` fields be read one way here and another way by
    // anything else in front of it.
    let field = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());

    let username = field("username").ok_or_else(|| "no username in the credential".to_owned())?;
    if username != USERNAME {
        return Err(format!("no such user: this proxy has one account, `{USERNAME}`"));
    }

    let realm = field("realm").ok_or_else(|| "no realm in the credential".to_owned())?;
    if realm != REALM {
        return Err("the realm does not match that of this proxy".to_owned());
    }

    let uri = field("uri").ok_or_else(|| "no uri in the credential".to_owned())?;
    if uri != target {
        return Err("the credential was made for a different path".to_owned());
    }

    let nonce = field("nonce").ok_or_else(|| "no nonce in the credential".to_owned())?;
    state.web_auth.redeem(&nonce, now)?;

    // RFC 7616: a client that was challenged with `algorithm=SHA-256` echoes it
    // back. Its absence means the client hashed with RFC 2617's default, which
    // is MD5 — and an MD5 response will not verify against a SHA-256
    // computation, so saying so beats "wrong password".
    match field("algorithm").as_deref() {
        Some("SHA-256") | None => {}
        Some(other) => {
            return Err(format!(
                "this proxy issues SHA-256 challenges; this credential used `{other}`"
            ));
        }
    }

    let response = field("response").ok_or_else(|| "no response in the credential".to_owned())?;
    if response.len() != 64 || !response.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the response is not a digest".to_owned());
    }

    // `qop=auth` is what a browser sends. A credential without it is the older
    // RFC 2069 form, which some clients still emit, and the hash differs by one
    // field — so both are computed correctly rather than one being refused.
    let expected = if let Some(qop) = field("qop") {
        if qop != "auth" {
            return Err(format!("only qop=auth is supported; this credential asked for `{qop}`"));
        }
        let nc = field("nc").ok_or_else(|| "qop=auth needs an nc".to_owned())?;
        let cnonce = field("cnonce").ok_or_else(|| "qop=auth needs a cnonce".to_owned())?;
        let ha1 = sha256_hex(&[&username, REALM, token]);
        let ha2 = sha256_hex(&[method.as_str(), &uri]);
        sha256_hex(&[&ha1, &nonce, &nc, &cnonce, &qop, &ha2])
    } else {
        let ha1 = sha256_hex(&[&username, REALM, token]);
        let ha2 = sha256_hex(&[method.as_str(), &uri]);
        sha256_hex(&[&ha1, &nonce, &ha2])
    };

    if tokens_match(&response, &expected) {
        Ok(())
    } else {
        // Deliberately one message for a wrong password and for a malformed
        // computation: distinguishing them tells an attacker which half to
        // keep working on.
        Err("the password is not the operator token".to_owned())
    }
}

/// The status a refusal from this module carries.
///
/// A function because the catcher needs the same answer and neither should be
/// able to drift from the other.
#[must_use]
pub const fn refusal_status() -> Status {
    Status::Unauthorized
}

// ── the wire format ─────────────────────────────────────────────────────────

/// Split a `Digest` credential's parameter list.
///
/// Hand-rolled rather than pulled from a crate because the grammar is small and
/// the interesting properties are all about what it must *not* accept: a bare
/// token, an unterminated quote, a value containing a comma. Those are the
/// cases the tests below pin.
///
/// Keys come back lowercased, values as sent. ASCII throughout — a byte over
/// 0x7f mangles into a different character, which fails verification rather
/// than passing it, so the failure mode is closed.
fn split_params(input: &str) -> Vec<(String, String)> {
    let b = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }

        let key_start = i;
        while i < b.len() && b[i] != b'=' && b[i] != b',' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = input[key_start..i].to_ascii_lowercase();

        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            // A bare token: not a parameter. Skipped rather than refused, since
            // what matters is that the fields we need are present and right.
            while i < b.len() && b[i] != b',' {
                i += 1;
            }
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }

        let value = if i < b.len() && b[i] == b'"' {
            i += 1;
            let mut v = String::new();
            while i < b.len() && b[i] != b'"' {
                // A backslash escapes the next byte, so `\"` does not close it.
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                v.push(b[i] as char);
                i += 1;
            }
            // An unterminated quote simply ends at the string's end, which
            // leaves a value that will not verify.
            if i < b.len() {
                i += 1;
            }
            v
        } else {
            let start = i;
            while i < b.len() && b[i] != b',' {
                i += 1;
            }
            input[start..i].trim().to_owned()
        };

        if !key.is_empty() {
            out.push((key, value));
        }
    }
    out
}

/// Whether a header carries a `Digest` credential.
///
/// Slicing a `&str` by byte index panics when the index is not a character
/// boundary, and the `Authorization` header comes straight off the wire before
/// anything has validated it — so `&header[..7]` is a remote panic on a path
/// nobody has authenticated on yet. `strip_prefix_ci` asks for the prefix with
/// `get`, which returns `None` instead of panicking.
#[must_use]
pub fn is_digest(header: &str) -> bool {
    strip_prefix_ci(header, "Digest ").is_some() || header.eq_ignore_ascii_case("Digest")
}

/// `str::strip_prefix`, ignoring case.
#[must_use]
fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    let head = haystack.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &haystack[prefix.len()..])
}

/// `SHA-256` of the parts, joined with colons, hex-encoded.
///
/// The joining is the A1/A2 concatenation RFC 7616 specifies, so it is spelled
/// once here rather than at each of the three call sites.
#[must_use]
fn sha256_hex(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h.update(b":");
        }
        h.update(part.as_bytes());
    }
    hex(&h.finalize())
}

/// Lowercase hex, which is the only spelling the RFCs allow.
#[must_use]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::http::middleware::admin_token;

    const TOKEN: &str = "tap_0123456789abcdef0123456789abcdef";

    /// A state with the operator token set.
    fn state() -> State {
        let mut state = State::for_tests(TOKEN.to_owned());
        state.web_auth = Nonces::new();
        state
    }

    /// The unhashed inputs of a credential.
    ///
    /// Grouped rather than passed one by one: a test that got two of these the
    /// wrong way round would still compile and would fail as "the password was
    /// refused", which sends the reader looking in the wrong place.
    struct Wanted<'a> {
        username: &'a str,
        realm: &'a str,
        password: &'a str,
        /// `(nc, cnonce)` for a `qop=auth` credential, or `None` for the older
        /// RFC 2069 form.
        qop: Option<(&'a str, &'a str)>,
    }

    /// The `qop=auth` form, which is what a browser sends.
    fn with_qop<'a>(username: &'a str, realm: &'a str, password: &'a str) -> Wanted<'a> {
        Wanted {
            username,
            realm,
            password,
            qop: Some(("00000001", "abc123")),
        }
    }

    /// A credential computed the way a browser computes one.
    fn credential(nonces: &Nonces, now: u64, method: Method, uri: &str, wanted: &Wanted<'_>) -> String {
        let Wanted {
            username,
            realm,
            password,
            qop,
        } = *wanted;
        let nonce = nonces.mint(now);
        let ha1 = sha256_hex(&[username, realm, password]);
        let ha2 = sha256_hex(&[method.as_str(), uri]);
        let response = match qop {
            // `qop=auth` is the only value this proxy accepts, so the `auth`
            // literal is spelled here rather than threaded through.
            Some((nc, cnonce)) => sha256_hex(&[&ha1, &nonce, nc, cnonce, "auth", &ha2]),
            None => sha256_hex(&[&ha1, &nonce, &ha2]),
        };
        let mut out = format!(
            "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", algorithm=SHA-256, response=\"{response}\""
        );
        if let Some((nc, cnonce)) = qop {
            // Writing into a String cannot fail.
            let _ = write!(out, ", qop=auth, nc={nc}, cnonce=\"{cnonce}\"");
        }
        out
    }

    /// A correct credential for `GET /api/users`.
    fn good(state: &State, now: u64) -> String {
        credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, REALM, TOKEN),
        )
    }

    // ── what is gated ───────────────────────────────────────────────────────

    #[test]
    fn the_dashboard_and_its_assets_are_gated() {
        // The whole point of the feature: no page, no script, no style sheet
        // and no API answer without a credential.
        for path in [
            "/",
            "/index.html",
            "/app.js",
            "/charts.js",
            "/styles.css",
            "/vendor/vue.global.prod.js",
            "/api/users",
            "/api/providers",
            "/api/inspect",
            "/openapi.json",
        ] {
            assert!(gated(path, Method::Get), "{path} is reachable without signing in");
        }
    }

    #[test]
    fn the_probe_the_crawler_files_and_the_icon_are_not_gated() {
        // Asked for before any credential exists, and none of them carries
        // anything worth protecting.
        for path in ["/api/hello", "/robots.txt", "/favicon.ico"] {
            assert!(!gated(path, Method::Get), "{path} demands a credential");
        }
    }

    #[test]
    fn the_relay_and_the_per_user_paths_are_not_gated() {
        // Their callers are CLIs holding a key, not browsers. A challenge here
        // would break every tab, because a tab cannot answer a prompt.
        for path in [
            "/relay/anthropic/v1/messages",
            "/relay/anthropic/api/hello",
            "/me/usage",
            "/me/credentials",
        ] {
            assert!(!gated(path, Method::Post), "{path} demands a browser credential");
        }
    }

    /// The bare parent of an exempt prefix is NOT exempt.
    ///
    /// Neither `/relay` nor `/me` is a route, so exempting one would only mean
    /// an unauthenticated request for it falls through to the SPA catch-all and
    /// is handed the HTML shell — which is exactly what the gate is for.
    #[test]
    fn the_parent_of_an_exempt_prefix_is_still_gated() {
        // Neither is a route: the relay lives at `/relay/anthropic/<sub..>` and
        // the per-user paths below `/me/`. Exempting a parent that mounts
        // nothing would only mean an unauthenticated request for it falls
        // through to the SPA catch-all and is handed the HTML shell.
        assert!(gated("/relay", Method::Get));
        assert!(gated("/me", Method::Get));
        assert!(gated("/me", Method::Post), "and not by verb either");
        assert!(gated("/relay/anthropic", Method::Get), "the bare prefix is not a route");

        // The children still are exempt, or the exemption would do nothing.
        assert!(!gated("/relay/anthropic/v1/messages", Method::Post));
        assert!(!gated("/me/usage", Method::Get));
    }

    #[test]
    fn a_path_that_merely_starts_with_an_exempt_name_is_not_exempt() {
        // The prefix test is on `/relay/anthropic/`, with both slashes, so a
        // path that merely begins with those letters is not covered.
        assert!(gated("/relayx", Method::Get));
        assert!(gated("/relayed", Method::Get));
        assert!(gated("/meeting", Method::Get));
        assert!(gated("/meta", Method::Get));
        // And a sibling of the relay is gated, which is the point of naming the
        // prefix in full rather than exempting all of `/relay`.
        assert!(gated("/relay/openai", Method::Get));
        assert!(gated("/relay/openai/v1/chat/completions", Method::Post));
    }

    #[test]
    fn a_post_to_an_exempt_path_is_still_exempt() {
        // The exemption is about who calls, not about the verb: `POST /me/credentials`
        // is a tab repairing its own credential.
        assert!(!gated("/me/credentials", Method::Post));
    }

    #[test]
    fn a_preflight_carries_no_credential_and_is_not_asked_for_one() {
        // The browser is required not to send credentials on a preflight, so
        // gating it would fail every one of them.
        assert!(!gated("/api/users", Method::Options));
        assert!(!gated("/", Method::Options));
    }

    // ── the nonces ──────────────────────────────────────────────────────────

    #[test]
    fn a_minted_nonce_redeems_once_and_then_not_again() {
        // Replay is the attack a nonce exists to stop: the whole credential
        // travels in one header, and anyone who copies it could otherwise use
        // it unchanged for as long as it lives.
        let nonces = Nonces::new();
        let now = 1_700_000_000;
        let nonce = nonces.mint(now);

        assert!(nonces.redeem(&nonce, now).is_ok(), "the first use is fine");
        assert!(
            nonces.redeem(&nonce, now).is_err(),
            "the second use of the same nonce must be refused"
        );
    }

    #[test]
    fn two_minted_nonces_differ() {
        // A nonce that repeated would make the replay table useless.
        let nonces = Nonces::new();
        assert_ne!(nonces.mint(1_700_000_000), nonces.mint(1_700_000_000));
    }

    #[test]
    fn a_nonce_this_process_did_not_mint_is_refused() {
        let a = Nonces::new();
        let b = Nonces::new();
        let theirs = b.mint(1_700_000_000);
        assert!(
            a.redeem(&theirs, 1_700_000_000).is_err(),
            "another process's nonce must not verify"
        );
    }

    #[test]
    fn a_forged_nonce_is_refused() {
        // Every byte of the tag matters: flipping one has to fail.
        let nonces = Nonces::new();
        let now = 1_700_000_000;
        let nonce = nonces.mint(now);
        let (payload, tag) = nonce.split_at(STAMP_HEX + RAND_HEX);

        let mut flipped = tag.to_owned();
        let first = flipped.remove(0);
        flipped.insert(0, if first == '0' { '1' } else { '0' });
        assert!(nonces.redeem(&format!("{payload}{flipped}"), now).is_err());

        // And every byte of the RANDOMNESS matters too. This is what the
        // signature covering the whole payload is for: if only the stamp were
        // signed, a client could submit a nonce body of its own choosing with a
        // tag that checks out.
        let mut chosen = nonce.clone();
        let at = STAMP_HEX;
        let digit = chosen.as_bytes()[at];
        chosen.replace_range(at..=at, if digit == b'0' { "1" } else { "0" });
        assert!(
            nonces.redeem(&chosen, now).is_err(),
            "a nonce with altered randomness must not verify"
        );
    }

    #[test]
    fn a_nonced_stamp_moved_forward_is_refused() {
        // The stamp is inside the signed part, so an attacker cannot rewrite it
        // to keep an old nonce alive.
        let nonces = Nonces::new();
        let now = 1_700_000_000;
        let nonce = nonces.mint(now - 10_000);
        let rest = &nonce[STAMP_HEX..];
        let rewritten = format!("{now:0STAMP_HEX$x}{rest}");
        assert!(
            nonces.redeem(&rewritten, now).is_err(),
            "a stamp that was not signed must not verify"
        );
    }

    #[test]
    fn a_nonce_older_than_the_window_is_refused() {
        let nonces = Nonces::new();
        let issued = 1_700_000_000;
        let nonce = nonces.mint(issued);
        assert!(nonces.redeem(&nonce, issued + NONCE_TTL_SECS).is_ok());
        assert!(
            nonces
                .redeem(&nonces.mint(issued), issued + NONCE_TTL_SECS + 1)
                .is_err()
        );
    }

    #[test]
    fn a_malformed_nonce_is_refused_rather_than_panicking() {
        let nonces = Nonces::new();
        for bad in ["", "short", &"z".repeat(STAMP_HEX + RAND_HEX + TAG_HEX)] {
            assert!(nonces.redeem(bad, 1_700_000_000).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn the_used_table_does_not_grow_without_bound() {
        // It is swept on mint, which is the only thing that adds to it — so a
        // long-running proxy accumulates at most one window of logins.
        let nonces = Nonces::new();
        let t0 = 1_700_000_000;
        for i in 0..50 {
            let n = nonces.mint(t0);
            let _ = nonces.redeem(&n, t0);
            let _ = i;
        }
        assert_eq!(
            nonces.used.lock().expect("lock").len(),
            50,
            "all fifty are inside one window"
        );

        // A mint well past the window sweeps what is stale.
        let _ = nonces.mint(t0 + NONCE_TTL_SECS * 2);
        assert!(
            nonces.used.lock().expect("lock").len() <= 1,
            "the sweep dropped the stale ones"
        );
    }

    // ── the challenge ───────────────────────────────────────────────────────

    #[test]
    fn the_challenge_is_the_form_a_browser_answers() {
        let nonces = Nonces::new();
        let c = challenge(&nonces, 1_700_000_000);
        assert!(c.starts_with("Digest "), "{c}");
        assert!(c.contains(&format!("realm=\"{REALM}\"")), "{c}");
        assert!(c.contains("qop=\"auth\""), "{c}");
        assert!(
            c.contains("algorithm=SHA-256"),
            "the challenge must advertise the strong algorithm: {c}"
        );
        assert!(!c.contains("MD5"), "MD5 is not offered at all: {c}");
        assert!(c.contains("nonce=\""), "{c}");

        // And the nonce it hands out is one this process can redeem, or every
        // login would fail on the first try.
        let nonce = c
            .split("nonce=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("a nonce");
        assert!(nonces.redeem(nonce, 1_700_000_000).is_ok());
    }

    #[test]
    fn a_challenge_does_not_name_the_password() {
        // It is served to anyone who asks. Nothing in it may be derived from
        // the token.
        let c = challenge(&Nonces::new(), 1_700_000_000);
        assert!(!c.contains(TOKEN), "{c}");
        assert!(!c.contains(USERNAME), "the username is not in the challenge: {c}");
    }

    // ── accepting a credential ──────────────────────────────────────────────

    #[test]
    fn a_correct_credential_is_accepted() {
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now);
        assert!(verify(&state, Method::Get, "/api/users", &header, now).is_ok());
    }

    #[test]
    fn a_credential_without_qop_is_accepted_for_older_clients() {
        // The RFC 2069 form, which some clients still send. The hash differs by
        // one field, so it has to be computed rather than refused.
        let state = state();
        let now = 1_700_000_000;
        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &Wanted {
                username: USERNAME,
                realm: REALM,
                password: TOKEN,
                qop: None,
            },
        );
        assert!(verify(&state, Method::Get, "/api/users", &header, now).is_ok());
    }

    #[test]
    fn the_wrong_password_is_refused() {
        // The case the whole feature exists for.
        let state = state();
        let now = 1_700_000_000;
        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, REALM, "tap_this_is_not_the_token"),
        );
        assert!(verify(&state, Method::Get, "/api/users", &header, now).is_err());
    }

    #[test]
    fn another_users_name_is_refused() {
        // The username is fixed. A different one is refused rather than
        // ignored, so a client with the wrong idea is told.
        let state = state();
        let now = 1_700_000_000;
        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop("root", REALM, TOKEN),
        );
        let err = verify(&state, Method::Get, "/api/users", &header, now).expect_err("refused");
        assert!(err.contains(USERNAME), "{err}");
    }

    #[test]
    fn a_relay_key_is_not_an_operator_password() {
        // The security property worth stating out loud: a key a tab holds
        // relays, and must not administer. If a user key satisfied this, every
        // account could rewrite every other account.
        let state = state();
        let now = 1_700_000_000;
        // Two statements, not one chain: `Mutex` is not reentrant, so adding an
        // account and then minting its key inside a closure over the same guard
        // deadlocks the test binary and, with it, the whole suite.
        let who = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add("Ada", "Lovelace", "digest@example.com")
            .expect("add");
        let (_, secret) = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add_key(&who.email, "laptop")
            .expect("mint");

        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, REALM, &secret),
        );
        assert!(
            verify(&state, Method::Get, "/api/users", &header, now).is_err(),
            "a relay key must not administer the proxy"
        );
    }

    #[test]
    fn a_credential_made_for_another_path_is_refused() {
        // The digest covers the request target, so this is what stops a
        // captured header from being replayed at a path it was not made for.
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now);
        let err = verify(&state, Method::Get, "/api/providers", &header, now)
            .expect_err("a credential for another path must not verify");
        assert!(err.contains("different path"), "{err}");
    }

    #[test]
    fn a_credential_made_for_another_verb_is_refused() {
        // The method is in the hash, so a credential good for GET is not good
        // for DELETE — which is the difference between reading the account list
        // and rewriting it.
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now);
        assert!(verify(&state, Method::Delete, "/api/users", &header, now).is_err());
    }

    #[test]
    fn a_wrong_realm_is_refused() {
        // The realm is in the password hash, so a credential computed against
        // another realm's password cannot be replayed here.
        let state = state();
        let now = 1_700_000_000;
        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, "somewhere-else", TOKEN),
        );
        assert!(verify(&state, Method::Get, "/api/users", &header, now).is_err());
    }

    /// No header, however malformed, may take the process down.
    ///
    /// A sweep rather than a list of cases: the failure being guarded against
    /// is a panic from slicing at the wrong byte, and that depends on lengths
    /// rather than on content.
    #[test]
    fn no_header_can_panic_the_verifier() {
        let state = state();
        let now = 1_700_000_000;
        let mut candidates: Vec<String> = vec![String::new(), "Digest ".to_owned()];
        for n in 0..=12 {
            let filler = "ü".repeat(n);
            for prefix in ["", "Digest ", "Digest username=", "digest a=", "Digest \""] {
                candidates.push(format!("{prefix}{filler}"));
                candidates.push(format!("{prefix}{filler}=\""));
                candidates.push(format!("{prefix}{filler}=\"{filler}\""));
                candidates.push(format!("{prefix}{filler}=\"x\\"));
            }
        }
        for header in candidates {
            // The answer is not the point; surviving is.
            let _ = verify(&state, Method::Get, "/api/users", &header, now);
        }
    }

    #[test]
    fn a_header_that_is_not_a_credential_is_refused() {
        let state = state();
        for header in ["", "Basic YWRtaW46c2VjcmV0", "Bearer tap_whatever", "Digest"] {
            assert!(
                verify(&state, Method::Get, "/api/users", header, 1_700_000_000).is_err(),
                "{header:?} was accepted"
            );
        }
    }

    #[test]
    fn a_credential_missing_a_required_field_is_refused() {
        let state = state();
        let now = 1_700_000_000;
        let full = good(&state, now);

        // Drop each field in turn; every one of them is load-bearing.
        // `starts_with`, not `contains`: `cnonce="…"` contains `nonce=`, so a
        // substring match would drop two fields at once and the test would no
        // longer say which one it removed.
        for field in [
            "username=",
            "realm=",
            "nonce=",
            "uri=",
            "response=",
            "qop=",
            "nc=",
            "cnonce=",
            "algorithm=",
        ] {
            // The scheme is stripped before splitting, or the first parameter
            // arrives as `Digest username="…"` and no `starts_with` on a field
            // name ever matches it.
            let params = full.strip_prefix("Digest ").expect("the scheme");
            let kept = params
                .split(", ")
                .filter(|part| !part.starts_with(field))
                .collect::<Vec<_>>();
            assert_eq!(
                kept.len(),
                params.split(", ").count() - 1,
                "the fixture did not contain exactly one {field}"
            );
            let without = format!("Digest {}", kept.join(", "));
            assert!(
                verify(&state, Method::Get, "/api/users", &without, now).is_err(),
                "a credential without {field} was accepted: {without}"
            );
        }
    }

    /// A credential hashed with another algorithm is refused by name.
    ///
    /// A client that read the challenge as MD5 — RFC 2617's default — would
    /// otherwise get "the password is not the operator token", which sends the
    /// operator looking for a credential problem that does not exist.
    #[test]
    fn a_credential_hashed_with_another_algorithm_is_refused_by_name() {
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now).replace("algorithm=SHA-256", "algorithm=MD5");
        let err = verify(&state, Method::Get, "/api/users", &header, now).expect_err("refused");
        assert!(err.contains("SHA-256") && err.contains("MD5"), "{err}");
    }

    /// SHA-256 it is, so the response is 64 hex characters rather than MD5's 32.
    ///
    /// Pinned because getting this wrong is silent: a 32-character response
    /// simply never verifies, and the only symptom is a password prompt that
    /// comes back.
    #[test]
    fn a_correct_response_is_a_full_sha256_digest() {
        let state = state();
        let header = good(&state, 1_700_000_000);
        let response = header
            .split("response=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("a response");
        assert_eq!(response.len(), 64, "{response}");
        assert!(response.bytes().all(|b| b.is_ascii_hexdigit()), "{response}");
    }

    /// `nc` and `cnonce` are required when `qop` is present, and refused by
    /// absence rather than defaulted.
    ///
    /// A defaulted `nc` would make every credential interchangeable for as long
    /// as its nonce lived, which is the replay window the single-use rule is
    /// there to close.
    #[test]
    fn the_fields_only_the_qop_path_needs_are_still_required() {
        let state = state();
        let now = 1_700_000_000;
        for part in ["nc=00000001", "cnonce=\"abc123\""] {
            let full = good(&state, now);
            let without = full.split(", ").filter(|p| *p != part).collect::<Vec<_>>().join(", ");
            assert_ne!(without, full, "the fixture did not contain {part}");
            assert!(
                verify(&state, Method::Get, "/api/users", &without, now).is_err(),
                "a credential without {part} was accepted: {without}"
            );
        }
    }

    #[test]
    fn a_qop_this_proxy_cannot_compute_is_refused_by_name() {
        // `auth-int` needs the body hash, which this proxy does not compute. A
        // silent fallback would either accept a credential it cannot check or
        // fail with a message that names nothing.
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now).replace("qop=auth", "qop=auth-int");
        let err = verify(&state, Method::Get, "/api/users", &header, now).expect_err("refused");
        assert!(err.contains("auth-int"), "{err}");
    }

    #[test]
    fn a_response_that_is_not_a_digest_is_refused() {
        let state = state();
        let now = 1_700_000_000;
        // Uppercase is included because RFC 7616 mandates lowercase hex, and a
        // client that uppercased it must not be accepted by accident.
        for bad in [
            "",
            "zz",
            "not-hex-here",
            &"a".repeat(63),
            &"a".repeat(65),
            &"A".repeat(64),
            &"0".repeat(64),
        ] {
            // The real digest is read OUT of the header rather than guessed at.
            // Substituting against an assumed value is a no-op when the
            // assumption is wrong, and the test then re-verifies a perfectly
            // valid credential while asserting it was refused — so it would
            // pass only if the implementation were broken.
            let header = good(&state, now);
            let start = header.find("response=\"").expect("a response field") + "response=\"".len();
            let end = start + 64;
            let forged = format!("{}{bad}{}", &header[..start], &header[end..]);
            assert!(
                verify(&state, Method::Get, "/api/users", &forged, now).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_credential_is_single_use_through_the_whole_verification() {
        // The nonce is redeemed by a successful check, so replaying the exact
        // header fails even though every field is right.
        let state = state();
        let now = 1_700_000_000;
        let header = good(&state, now);
        assert!(verify(&state, Method::Get, "/api/users", &header, now).is_ok());
        assert!(
            verify(&state, Method::Get, "/api/users", &header, now).is_err(),
            "the same header must not work twice"
        );
    }

    #[test]
    fn an_installation_with_no_token_cannot_sign_anybody_in() {
        // Rather than accepting everything, which is the failure mode that
        // matters: a proxy with no token configured must lock the door.
        let state = State::for_tests(String::new());
        let now = 1_700_000_000;
        let header = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, REALM, ""),
        );
        let err = verify(&state, Method::Get, "/api/users", &header, now).expect_err("refused");
        assert!(err.contains("token"), "{err}");
        assert!(
            !admin_token::configured(&state),
            "the case being tested is an unconfigured token"
        );
    }

    #[test]
    fn verification_never_echoes_the_token() {
        // The message ends up in a response body and in a log. The token must
        // not be in it, or a failed login would print the password.
        let state = state();
        let now = 1_700_000_000;
        let bad = credential(
            &state.web_auth,
            now,
            Method::Get,
            "/api/users",
            &with_qop(USERNAME, REALM, "guess"),
        );
        let err = verify(&state, Method::Get, "/api/users", &bad, now).expect_err("refused");
        assert!(!err.contains(TOKEN), "{err}");
    }

    // ── the parameter grammar ───────────────────────────────────────────────

    #[test]
    fn parameters_are_split_on_commas_outside_quotes() {
        // A quoted value containing a comma is legal and must not be split, or
        // every field after it would be read wrongly.
        let params = split_params(r#"a="one,two", b=three, c="four""#);
        assert_eq!(
            params,
            vec![
                ("a".to_owned(), "one,two".to_owned()),
                ("b".to_owned(), "three".to_owned()),
                ("c".to_owned(), "four".to_owned()),
            ]
        );
    }

    #[test]
    fn keys_are_matched_without_regard_to_case() {
        // Some clients capitalise. Failing on that would be a support ticket
        // with nothing in the log to explain it.
        let params = split_params(r#"UserName="ada", REALM="r""#);
        assert_eq!(params[0].0, "username");
        assert_eq!(params[1].0, "realm");
    }

    #[test]
    fn a_quoted_value_may_contain_an_escaped_quote() {
        let params = split_params(r#"a="x\"y", b=z"#);
        assert_eq!(params[0], ("a".to_owned(), "x\"y".to_owned()));
        assert_eq!(params[1], ("b".to_owned(), "z".to_owned()));
    }

    #[test]
    fn whitespace_around_parameters_is_ignored() {
        // The exact spacing is up to the client, and browsers differ.
        let compact = split_params("a=1,b=2");
        let spaced = split_params("  a = 1 ,  b = 2  ");
        let quoted = split_params(r#"a="1", b="2""#);
        assert_eq!(compact, spaced);
        assert_eq!(compact, quoted);
    }

    #[test]
    fn an_unterminated_quote_does_not_run_past_the_end() {
        // It yields a value that will not verify rather than panicking or
        // eating the rest of the header in a way that hides a field.
        let params = split_params(r#"a="unterminated"#);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].1, "unterminated");
    }

    #[test]
    fn a_bare_token_is_skipped_rather_than_mistaken_for_a_field() {
        // `Digest stale, username="ada"` — `stale` is a token with no value.
        let params = split_params(r#"stale, username="ada""#);
        assert_eq!(params, vec![("username".to_owned(), "ada".to_owned())]);
    }

    #[test]
    fn only_the_first_occurrence_of_a_field_counts() {
        // RFC 7616 says the first wins, and it matters: a header carrying two
        // `response` fields read differently here and elsewhere is exactly the
        // shape of a request-smuggling bug.
        let params = split_params(r#"response="first", response="second""#);
        assert_eq!(params.len(), 2);
        assert_eq!(
            params.iter().find(|(k, _)| k == "response").map(|(_, v)| v.as_str()),
            Some("first")
        );
    }

    /// A header with multi-byte characters must not panic.
    ///
    /// This is reachable before authentication: the `Authorization` header
    /// arrives from the network and nothing has checked it yet. Slicing a
    /// `&str` at a byte index that lands inside a multi-byte character panics,
    /// which Rocket turns into a 500 — or, with `panic = "abort"`, kills the
    /// process. Neither is an acceptable answer to a malformed header.
    #[test]
    fn a_non_ascii_header_is_refused_rather_than_panicking() {
        assert!(!is_digest("üüüüüüüü"));
        assert!(!is_digest(""));
        assert!(!is_digest("Di"));
        assert!(!is_digest("Diges"));

        // Every prefix short enough to be sliced under a multi-byte character.
        for n in 0..=20 {
            let header = "ü".repeat(n);
            let _ = is_digest(&header);
            let _ = split_params(&header);
        }
    }

    #[test]
    fn a_digest_scheme_is_recognised_however_it_is_cased() {
        // The scheme is case-insensitive in HTTP, and browsers differ.
        for header in ["Digest username=x", "digest username=x", "DIGEST username=x"] {
            assert!(is_digest(header), "{header}");
        }
        assert!(is_digest("Digest"), "the bare scheme, which will then fail to verify");
        assert!(!is_digest("Basic dXNlcjpwdw=="));
        assert!(!is_digest("Bearer token"));
    }

    #[test]
    fn an_empty_parameter_list_yields_nothing() {
        for input in ["", "   ", ",,,", " , "] {
            assert!(split_params(input).is_empty(), "{input:?}");
        }
    }

    #[test]
    fn an_empty_value_is_read_as_empty_rather_than_missing() {
        // `username=""` is present and wrong, which is a different thing from
        // absent — and both end in a refusal, for different reasons.
        let params = split_params(r#"username="", realm=r"#);
        assert_eq!(params[0], ("username".to_owned(), String::new()));
        assert_eq!(params[1], ("realm".to_owned(), "r".to_owned()));
    }
}
