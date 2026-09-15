// SPDX-License-Identifier: MPL-2.0

//! The browser gate, against the real binary over a real socket.
//!
//! Unit tests can prove the digest arithmetic; only this can prove the thing
//! that was actually asked for — that a client which cannot answer the
//! challenge receives **no page, no script, no style sheet and no API answer**,
//! while the three paths that have to stay reachable still are.
//!
//! It runs `tab-atelier-proxy` as a process for the same reason
//! `end_to_end.rs` does: the guards, the catchers and the route table only
//! meet each other inside a running server, and whether the challenge reaches
//! the wire is not visible from a library call.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

/// The binary under test, built by cargo for this integration target.
const BIN: &str = env!("CARGO_BIN_EXE_tab-atelier-proxy");

/// A directory that cleans itself up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("ta-browser-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("home/.claude")).expect("mkdir");
        // A far-future expiry so the egress never reaches the real OAuth
        // endpoint while these tests run.
        std::fs::write(
            p.join("home/.claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"oat","refreshToken":"r","expiresAt":9999999999999,"scopes":[]}}"#,
        )
        .expect("write creds");
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child killed when the test ends, however it ends.
struct Serving(Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run the CLI the way an operator would, on the same directories the server
/// will get — the two disagreeing is the bug this harness exists to catch.
fn cli(scratch: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .env("TAB_ATELIER_PROXY_CONFIG", scratch.join("config"))
        .env("TAB_ATELIER_PROXY_STATE", scratch.join("state"))
        .env("HOME", scratch.join("home"))
        .output()
        .expect("run the CLI");
    assert!(
        out.status.success(),
        "`{}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

/// A running proxy, owning everything it needs to keep running.
///
/// One struct rather than a tuple because the scratch directory has to outlive
/// the server: dropping it deletes the state the server is still writing to.
/// Returning it detached from the child is how the first version of this file
/// removed its own working directory mid-test.
struct Proxy {
    #[allow(dead_code, reason = "held for its Drop: it deletes the directory")]
    scratch: Scratch,
    #[allow(dead_code, reason = "held for its Drop: it kills the server")]
    child: Serving,
    port: u16,
    token: String,
}

impl Proxy {
    /// Start on a fresh scratch directory, with the real assets behind it.
    fn start(name: &str) -> Self {
        Self::start_with(Scratch::new(name))
    }

    /// Start against a directory the caller has already prepared.
    ///
    /// This is how a test gets an installation with accounts or keys in it
    /// before the server comes up — the CLI writes the same files the server
    /// reads, so arranging state means running the CLI first.
    fn start_with(scratch: Scratch) -> Self {
        let port = free_port();
        let token = cli(scratch.path(), &["admin-token"]).trim().to_owned();
        assert!(token.starts_with("tap_"), "unexpected token: {token}");

        let child = Command::new(BIN)
            .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
            .env("TAB_ATELIER_PROXY_CONFIG", scratch.path().join("config"))
            .env("TAB_ATELIER_PROXY_STATE", scratch.path().join("state"))
            .env("HOME", scratch.path().join("home"))
            // The repository's own assets, so `/` serves a real page rather than
            // whatever the installed package happens to hold.
            .env(
                "TAB_ATELIER_PROXY_WEB",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
            )
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the proxy");

        let mut child = Serving(child);
        wait_until_listening(port, &mut child.0);
        Self {
            scratch,
            child,
            port,
            token,
        }
    }
}

/// Block until the port answers, or the server dies trying.
fn wait_until_listening(port: u16, child: &mut Child) {
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            panic!("the proxy exited ({status}) before listening: {err}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the proxy never listened on {port}");
}

/// One request, as raw bytes, with an explicit `Connection: close`.
///
/// Raw rather than through a client library so that a malformed or unusual
/// header can be sent exactly as written — which is the point of several of
/// these tests.
fn http(port: u16, req: &str) -> String {
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("timeout");
    sock.write_all(req.as_bytes()).expect("write");
    sock.flush().expect("flush");
    let mut out = Vec::new();
    let _ = sock.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// A plain GET with no credential.
fn get(port: u16, path: &str) -> String {
    http(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    )
}

/// The status code from a raw response.
fn status(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in: {}", &response[..response.len().min(200)]))
}

/// A response header, lowercased name, case-insensitive lookup.
fn header(response: &str, name: &str) -> Option<String> {
    let head = response.split("\r\n\r\n").next()?;
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
    })
}

/// The body of a raw response.
fn body(response: &str) -> String {
    response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default()
}

/// The `nonce` from a `WWW-Authenticate: Digest` challenge.
fn nonce_of(response: &str) -> String {
    let challenge = header(response, "www-authenticate").expect("a challenge header");
    challenge
        .split("nonce=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap_or_else(|| panic!("no nonce in {challenge}"))
        .to_owned()
}

/// SHA-256 as lowercase hex, which is what RFC 7616 requires.
///
/// Hand-rolled rather than pulled in: the test crate has no hex dependency and
/// adding one to spell sixteen bytes would be the tail wagging the dog.
fn sha256(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.finalize().iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// A `Digest` credential, computed the way a browser computes one.
///
/// `target` is the path and query exactly as the request line carries it: the
/// digest covers the request-target, so passing anything else produces a
/// credential the server should refuse.
fn credential(token: &str, method: &str, target: &str, nonce: &str) -> String {
    let nc = "00000001";
    let cnonce = "0a4f113b";
    let ha1 = sha256(&format!("admin:tab-atelier:{token}"));
    let ha2 = sha256(&format!("{method}:{target}"));
    let response = sha256(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
    format!(
        "Digest username=\"admin\", realm=\"tab-atelier\", nonce=\"{nonce}\", uri=\"{target}\", \
         algorithm=SHA-256, response=\"{response}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\""
    )
}

/// A GET carrying a freshly computed credential for `path`.
fn signed_get(port: u16, path: &str, token: &str) -> String {
    // First ask for a nonce. A browser does this too — the challenge is what
    // tells it which algorithm and realm to hash against.
    let challenge = get(port, path);
    assert_eq!(status(&challenge), 401, "expected a challenge for {path}");
    let credential = credential(token, "GET", path, &nonce_of(&challenge));
    http(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n"),
    )
}

/// The paths that must not be reachable without signing in.
///
/// This is the list from the request, and it is the whole feature: if any of
/// these answers without a credential, a scraper has something to read.
const GATED: [&str; 13] = [
    "/",
    "/index.html",
    // The real asset names. There is no `/styles.css`: the only style sheet is
    // the distribution's bootstrap, which the web controller falls back to
    // under `/usr/share/javascript/`.
    "/vendor/bootstrap.min.css",
    "/app.js",
    "/charts.js",
    "/vendor/vue.global.prod.js",
    "/api/users",
    "/api/providers",
    "/api/usage",
    "/api/inspect",
    "/api/pressure",
    // The bare parents of the two exempt prefixes. Neither is a route, so
    // exempting one would only mean an unauthenticated request falls through to
    // the SPA catch-all and is handed the HTML shell — which is precisely what
    // the gate is for.
    "/me",
    "/relay",
];

/// Nothing on the gated list may be read without a credential.
#[test]
fn nothing_serves_a_page_or_a_script_without_signing_in() {
    let proxy = Proxy::start("gated");
    let port = proxy.port;

    for path in GATED {
        let response = get(port, path);
        assert_eq!(
            status(&response),
            401,
            "{path} answered without a credential:\n{}",
            &response[..response.len().min(300)]
        );
        // And no content leaked in the refusal, which is what a scraper would
        // settle for.
        let body = body(&response);
        assert!(
            !body.contains("<html") && !body.contains("function") && !body.contains("/*"),
            "{path} leaked content in its 401: {}",
            &body[..body.len().min(200)]
        );
    }
}

/// The refusal is a challenge a browser can answer.
#[test]
fn the_refusal_is_a_digest_challenge_a_browser_answers() {
    let proxy = Proxy::start("challenge");
    let port = proxy.port;
    let response = get(port, "/");

    assert_eq!(status(&response), 401);
    let challenge = header(&response, "www-authenticate").expect("a WWW-Authenticate header");

    assert!(challenge.starts_with("Digest "), "{challenge}");
    assert!(challenge.contains("realm=\"tab-atelier\""), "{challenge}");
    assert!(challenge.contains("qop=\"auth\""), "{challenge}");
    assert!(
        challenge.contains("algorithm=SHA-256"),
        "the strong algorithm must be advertised: {challenge}"
    );
    assert!(
        !challenge.contains("MD5"),
        "MD5 must not be offered at all: {challenge}"
    );
    assert!(!nonce_of(&response).is_empty());
    // And the token is not in it; the challenge is served to anyone who asks.
    assert!(!challenge.contains("tap_"), "{challenge}");
}

/// A correct credential opens the page and everything it needs.
///
/// The interesting part is that the *assets* come through the same gate as the
/// page: a credential good for one is good for the others, which is what makes
/// the dashboard actually load.
#[test]
fn a_correct_credential_serves_the_page_and_its_assets() {
    let proxy = Proxy::start("signed-in");
    let port = proxy.port;
    let token = &proxy.token;

    for path in [
        "/",
        "/index.html",
        "/app.js",
        "/charts.js",
        "/vendor/vue.global.prod.js",
    ] {
        let response = signed_get(port, path, token);
        assert_eq!(
            status(&response),
            200,
            "{path} was refused a correctly signed request:\n{}",
            &response[..response.len().min(300)]
        );
        assert!(!body(&response).is_empty(), "{path} answered 200 with no content");
    }

    // The page is the real one, not a placeholder.
    let page = signed_get(port, "/", token);
    assert!(body(&page).contains("<html"), "the page is not HTML");
}

/// Every asset comes back with the content type a browser will act on.
///
/// Asserting the bytes arrive is not enough: a reply's *type* is what decides
/// whether the browser executes it. A `.js` served as `application/json` is
/// refused outright when the response says not to sniff, and the dashboard
/// renders blank with one console line and no clue which layer was at fault —
/// which is exactly what happened when the `Responder` adapter overrode the
/// type the web controller had worked out. This is the check that catches it,
/// and it is a wire-level one because a unit test of the adapter did not.
#[test]
fn each_asset_is_served_with_the_type_a_browser_needs() {
    let proxy = Proxy::start("content-types");
    let port = proxy.port;
    let token = &proxy.token;

    // The gated assets: these need a credential, so they are fetched with one.
    for (path, want) in [
        ("/", "text/html"),
        ("/index.html", "text/html"),
        ("/app.js", "application/javascript"),
        ("/charts.js", "application/javascript"),
        ("/vendor/vue.global.prod.js", "application/javascript"),
    ] {
        let response = signed_get(port, path, token);
        assert_eq!(status(&response), 200, "{path} was refused");
        let got = header(&response, "content-type").unwrap_or_else(|| panic!("{path} answered with no content type"));
        assert!(
            got.starts_with(want),
            "{path} was served as `{got}` rather than `{want}` — a browser will refuse it"
        );
        assert_ne!(
            got, "application/json",
            "{path} was served as JSON, which is the adapter overriding the handler"
        );
    }

    // And the two exempt ones, which are fetched without a credential — so
    // `signed_get` would fail on them before the type was ever seen.
    for (path, want) in [("/robots.txt", "text/plain"), ("/favicon.ico", "image/x-icon")] {
        let response = get(port, path);
        assert_eq!(status(&response), 200, "{path}");
        let got = header(&response, "content-type").unwrap_or_else(|| panic!("{path} answered with no content type"));
        assert!(
            got.starts_with(want),
            "{path} was served as `{got}` rather than `{want}`"
        );
    }
}

/// The API is behind the same credential, and answers with data.
#[test]
fn the_api_opens_with_the_same_credential_as_the_page() {
    let proxy = Proxy::start("api");
    let port = proxy.port;
    let token = &proxy.token;
    let response = signed_get(port, "/api/users", token);

    assert_eq!(status(&response), 200);
    let parsed: serde_json::Value = serde_json::from_str(&body(&response)).expect("JSON");
    assert!(parsed.get("users").is_some(), "{parsed}");
}

/// The operator token in a header still works, for scripts and the CLI.
///
/// A command-line client cannot answer a prompt, so the token has to be
/// acceptable directly — the same secret, presented differently.
#[test]
fn the_operator_token_is_accepted_without_a_challenge() {
    let proxy = Proxy::start("token-header");
    let port = proxy.port;
    let token = &proxy.token;

    // The two the server actually reads (`arrival::presented`). An earlier
    // version of this test used `x-tab-atelier-token`, which nothing reads —
    // so it was testing the test's own invention rather than the server.
    for header_line in [format!("Authorization: Bearer {token}"), format!("x-api-key: {token}")] {
        let response = http(
            port,
            &format!("GET /api/users HTTP/1.1\r\nHost: x\r\n{header_line}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status(&response), 200, "{header_line} was refused");
    }
}

/// A relay key is not an operator credential.
///
/// The security property worth proving end to end: a key a tab holds relays,
/// and must not administer. If one satisfied this gate, every user could
/// rewrite every account.
#[test]
fn a_relay_key_does_not_open_the_operator_surface() {
    // Arrange the account and its key with the CLI before the server starts:
    // the CLI writes the same files the server reads.
    let scratch = Scratch::new("relay-key");
    cli(scratch.path(), &["add", "Ada", "Lovelace", "ada@example.com"]);
    // The subcommands take positional arguments. `add` deliberately prints no
    // key — a key is named after the machine it lives on, so `add-key` is the
    // step that mints one.
    let minted = cli(scratch.path(), &["add-key", "ada@example.com", "laptop"]);
    let proxy = Proxy::start_with(scratch);
    let port = proxy.port;

    // Read by its marker rather than by prefix: the operator token uses the
    // same `tap_` prefix, so a prefix match would pick up either depending on
    // the order they happened to appear in.
    let key = minted
        .lines()
        .find_map(|l| l.trim().strip_prefix("key: "))
        .unwrap_or_else(|| panic!("no key in the CLI output: {minted}"))
        .trim()
        .to_owned();
    assert!(key.starts_with("tap_"), "unexpected key: {key}");
    assert_ne!(key, proxy.token, "the key must not be the operator token");

    // Presented as a browser credential, it must fail: the digest is computed
    // against the operator token, so a user key simply does not produce a match.
    let challenge = get(port, "/api/users");
    assert_eq!(status(&challenge), 401);
    let forged = credential(&key, "GET", "/api/users", &nonce_of(&challenge));
    let response = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: {forged}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(
        status(&response),
        401,
        "a user's relay key must not administer the proxy"
    );

    // And the same key IS good for the relay, which is what it is for. Without
    // this the test above would pass even if the key were simply invalid.
    //
    // The assertion is on the proxy's own wording rather than on the status:
    // this request goes on to a provider, and a provider is entitled to answer
    // 401 — Anthropic does, for the fake token this fixture uses — so a status
    // check would confuse "the proxy refused the key" with "the provider
    // refused the token". `no valid key` is a string only this proxy produces.
    let relay = http(
        port,
        &format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    );
    assert!(
        !body(&relay).contains("no valid key"),
        "the relay refused a valid key: {}",
        &relay[..relay.len().min(300)]
    );
}

/// A credential with an explicit nonce count.
///
/// A browser is given ONE nonce and reuses it for every request it then makes,
/// incrementing `nc` each time so the server can tell a repeat from a new
/// request. Computing `nc` differently is how a test reproduces that.
fn credential_with_nc(token: &str, method: &str, target: &str, nonce: &str, nc: &str) -> String {
    let cnonce = "0b5f9a2c";
    let ha1 = sha256(&format!("admin:tab-atelier:{token}"));
    let ha2 = sha256(&format!("{method}:{target}"));
    let response = sha256(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
    format!(
        "Digest username=\"admin\", realm=\"tab-atelier\", nonce=\"{nonce}\", uri=\"{target}\", \
         algorithm=SHA-256, response=\"{response}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\""
    )
}

/// One challenge, then every asset fetched against that same nonce.
///
/// This is what a browser actually does, and it is what was broken: the proxy
/// treated a nonce as single-use, so the first asset loaded and every one after
/// it was refused with "that nonce has already been used". The dashboard
/// rendered as a bare page with a broken script, and the only visible clue was
/// a set of 401s for files that had just been offered.
///
/// RFC 7616 is explicit that the client gets one nonce per realm and reuses it,
/// with `nc` counting the uses — so refusing a counter it has not seen is
/// refusing correct behaviour.
#[test]
fn one_nonce_serves_every_asset_a_page_needs() {
    let proxy = Proxy::start("nonce-reuse");
    let port = proxy.port;
    let token = &proxy.token;

    // The browser asks for the page, is challenged once, and keeps that nonce.
    let challenge = get(port, "/");
    assert_eq!(status(&challenge), 401, "the page is gated");
    let nonce = nonce_of(&challenge);

    for (i, path) in ["/", "/app.js", "/charts.js", "/index.html"].iter().enumerate() {
        let nc = format!("{:08x}", i + 1);
        let credential = credential_with_nc(token, "GET", path, &nonce, &nc);
        let response = http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\n\
                 Connection: close\r\n\r\n"
            ),
        );
        assert_eq!(
            status(&response),
            200,
            "{path} at nc={nc} was refused though the nonce was issued for this page: {}",
            &response[..response.len().min(240)]
        );
    }
}

/// A nonce the server never issued is refused, and the message says so.
///
/// The refusal has to name the nonce rather than the password: they have
/// different causes, and a proxy that reports "wrong password" for a bad nonce
/// sends the operator to reset a credential that was never the problem. The
/// expiry half of this rule is a unit test (`a_nonce_is_not_eternal`), since an
/// aged nonce cannot be minted from outside the process.
#[test]
fn a_nonce_this_proxy_never_issued_is_refused_by_name() {
    let proxy = Proxy::start("nonce-forged");
    let port = proxy.port;
    let token = &proxy.token;

    // Correctly framed at the right length, so it is refused for being
    // unrecognised rather than for being malformed.
    let forged = credential_with_nc(token, "GET", "/app.js", &"a".repeat(64), "00000001");
    let response = http(
        port,
        &format!("GET /app.js HTTP/1.1\r\nHost: x\r\nAuthorization: {forged}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status(&response), 401);
    let text = body(&response);
    assert!(
        text.contains("nonce"),
        "the refusal should name the nonce as the problem: {text}"
    );
}

/// Replaying a captured credential fails.
#[test]
fn a_captured_credential_cannot_be_replayed() {
    let proxy = Proxy::start("replay");
    let port = proxy.port;
    let token = &proxy.token;

    let challenge = get(port, "/api/users");
    let credential = credential(token, "GET", "/api/users", &nonce_of(&challenge));
    let request =
        format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n");

    assert_eq!(status(&http(port, &request)), 200, "the first use works");
    assert_eq!(
        status(&http(port, &request)),
        401,
        "the same credential must not work twice"
    );
}

/// A credential is bound to the path it was made for.
#[test]
fn a_credential_made_for_one_path_does_not_open_another() {
    let proxy = Proxy::start("path-bound");
    let port = proxy.port;
    let token = &proxy.token;

    let challenge = get(port, "/api/users");
    // Computed for `/`, then sent at `/api/users`.
    let credential = credential(token, "GET", "/", &nonce_of(&challenge));
    let response = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status(&response), 401, "a credential for another path must not verify");
}

/// A credential made for another verb does not open this one.
#[test]
fn a_credential_made_for_a_get_does_not_authorize_a_delete() {
    let proxy = Proxy::start("verb-bound");
    let port = proxy.port;
    let token = &proxy.token;

    let challenge = get(port, "/api/users/nobody");
    let credential = credential(token, "GET", "/api/users/nobody", &nonce_of(&challenge));
    let response = http(
        port,
        &format!(
            "DELETE /api/users/nobody HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\n\
             Connection: close\r\n\r\n"
        ),
    );
    assert_eq!(status(&response), 401, "the verb is part of the digest");
}

/// The query string is part of the signed target.
///
/// A client that hashed only the path would be refused, and a server that
/// verified only the path would let a credential be reused against a different
/// query — so this pins both halves.
#[test]
fn the_query_string_is_part_of_the_signed_target() {
    let proxy = Proxy::start("query-bound");
    let port = proxy.port;
    let token = &proxy.token;

    let target = "/api/usage?window=24h";
    let response = signed_get(port, target, token);
    assert_eq!(
        status(&response),
        200,
        "a credential hashing the whole target must be accepted"
    );

    // And one that hashed only the path must not be.
    let challenge = get(port, target);
    let path_only = credential(token, "GET", "/api/usage", &nonce_of(&challenge));
    let response = http(
        port,
        &format!("GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: {path_only}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(
        status(&response),
        401,
        "a credential that ignored the query must not verify"
    );
}

/// The three paths that must stay reachable still are.
#[test]
fn the_crawler_files_the_probe_and_the_relay_are_not_gated() {
    let proxy = Proxy::start("exempt");
    let port = proxy.port;

    for path in ["/robots.txt", "/favicon.ico", "/api/hello"] {
        let response = get(port, path);
        assert_eq!(
            status(&response),
            200,
            "{path} must be reachable without a credential:\n{}",
            &response[..response.len().min(300)]
        );
        assert!(
            header(&response, "www-authenticate").is_none(),
            "{path} must not be challenged"
        );
    }

    // The icon is actually an icon, not an empty 200.
    let icon = get(port, "/favicon.ico");
    assert!(
        header(&icon, "content-type").is_some_and(|t| t.contains("icon") || t.contains("octet-stream")),
        "the favicon has no usable content type"
    );
    assert!(!body(&icon).is_empty(), "the favicon is empty");

    // The relay answers its own probe without a browser credential, which is
    // what a tab checks before it starts.
    let relay = get(port, "/relay/anthropic/api/hello");
    assert_eq!(
        status(&relay),
        200,
        "a tab cannot answer a prompt, so the relay must not issue one"
    );
    assert!(
        header(&relay, "www-authenticate").is_none(),
        "no browser challenge may be sent to a CLI"
    );
}

/// A relay request with no key is refused as a key problem, not a browser one.
///
/// The distinction matters: a challenge here would make the client try to
/// interpret a browser flow, and the operator would see a prompt-shaped error
/// for what is actually a missing key.
#[test]
fn a_relay_request_without_a_key_is_not_challenged_as_a_browser() {
    let proxy = Proxy::start("relay-refusal");
    let port = proxy.port;
    let response = http(
        port,
        "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\n\
         Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status(&response), 401);
    assert!(
        header(&response, "www-authenticate").is_none(),
        "a CLI must not be handed a browser challenge"
    );
}

/// A CORS preflight is not challenged.
///
/// The browser is required not to send credentials on a preflight, so gating it
/// would fail every one of them and break the development UI with a CORS error
/// that names nothing.
#[test]
fn a_preflight_is_answered_without_a_credential() {
    let proxy = Proxy::start("preflight");
    let port = proxy.port;
    let response = http(
        port,
        "OPTIONS /api/users HTTP/1.1\r\nHost: x\r\nOrigin: http://localhost:5173\r\n\
         Access-Control-Request-Method: GET\r\nConnection: close\r\n\r\n",
    );
    assert_ne!(status(&response), 401, "a preflight must not be challenged");
    assert!(
        header(&response, "access-control-allow-origin").is_some(),
        "and it must carry the headers the browser asked for"
    );
}

/// A malformed `Authorization` header is refused, not fatal.
///
/// This is reachable before authentication: the header comes off the wire and
/// nothing has validated it. Slicing a `&str` at a byte index inside a
/// multi-byte character panics, so a non-ASCII header is the case that matters
/// — it must produce a 401, not a 500 and not a dead process.
#[test]
fn a_malformed_authorization_header_is_refused_rather_than_fatal() {
    let proxy = Proxy::start("malformed");
    let port = proxy.port;

    for header_line in [
        "Authorization: \u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}\u{fc}",
        "Authorization: Digest",
        "Authorization: Digest ",
        "Authorization: Digest username=\"\u{fc}\u{fc}\u{fc}\"",
        "Authorization: Digest \u{fc}\u{fc}\u{fc}=\"x\"",
        "Authorization: Digest username=\"admin\", realm=\"tab-atelier\", nonce=\"\"",
        "Authorization: Basic YWRtaW46cHc=",
        "Authorization: Digest username=\"admin\", realm=\"tab-atelier\", nonce=\"00\"",
    ] {
        let response = http(
            port,
            &format!("GET /api/users HTTP/1.1\r\nHost: x\r\n{header_line}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(
            status(&response),
            401,
            "{header_line:?} got {} rather than a refusal",
            status(&response)
        );
    }

    // And the server is still there afterwards, which a panic in a handler
    // would not necessarily leave true.
    assert_eq!(status(&get(port, "/api/hello")), 200);
}

/// The wrong password is refused, and said so.
#[test]
fn the_wrong_password_is_refused() {
    let proxy = Proxy::start("wrong-password");
    let port = proxy.port;

    let challenge = get(port, "/api/users");
    let credential = credential(
        "tap_not_the_real_token_at_all",
        "GET",
        "/api/users",
        &nonce_of(&challenge),
    );
    let response = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status(&response), 401);

    // The refusal must not hand back the token it expected.
    let scratch_token = "";
    let _ = scratch_token;
    assert!(
        !body(&response).contains("tap_0"),
        "the refusal echoed a token-ish string: {}",
        body(&response)
    );
}

/// The response is a well-formed JSON error, not Rocket's HTML page.
///
/// Every client of this proxy parses JSON, and the 401 is the one they will see
/// most often when something is misconfigured.
#[test]
fn a_refusal_is_json() {
    let proxy = Proxy::start("json-refusal");
    let port = proxy.port;
    let response = get(port, "/api/users");
    let body = body(&response);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("not JSON: {body}"));
    assert!(parsed.get("error").is_some(), "{parsed}");
}
