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

/// A server on a scratch directory, with the real assets behind it.
///
/// Returns the port and the operator token.
fn start(name: &str) -> (Serving, u16, String) {
    let scratch = Scratch::new(name);
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

    let mut serving = Serving(child);
    wait_until_listening(port, &mut serving.0);
    (serving, port, token)
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
const GATED: [&str; 11] = [
    "/",
    "/index.html",
    "/styles.css",
    "/app.js",
    "/charts.js",
    "/vendor/vue.global.prod.js",
    "/api/users",
    "/api/providers",
    "/api/usage",
    "/api/inspect",
    "/api/pressure",
];

/// Nothing on the gated list may be read without a credential.
#[test]
fn nothing_serves_a_page_or_a_script_without_signing_in() {
    let (_serving, port, _token) = start("gated");

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
    let (_serving, port, _token) = start("challenge");
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
    let (_serving, port, token) = start("signed-in");

    for path in [
        "/",
        "/index.html",
        "/styles.css",
        "/app.js",
        "/vendor/vue.global.prod.js",
    ] {
        let response = signed_get(port, path, &token);
        assert_eq!(
            status(&response),
            200,
            "{path} was refused a correctly signed request:\n{}",
            &response[..response.len().min(300)]
        );
        assert!(!body(&response).is_empty(), "{path} answered 200 with no content");
    }

    // The page is the real one, not a placeholder.
    let page = signed_get(port, "/", &token);
    assert!(body(&page).contains("<html"), "the page is not HTML");
}

/// The API is behind the same credential, and answers with data.
#[test]
fn the_api_opens_with_the_same_credential_as_the_page() {
    let (_serving, port, token) = start("api");
    let response = signed_get(port, "/api/users", &token);

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
    let (_serving, port, token) = start("token-header");

    for header_line in [
        format!("Authorization: Bearer {token}"),
        format!("x-tab-atelier-token: {token}"),
    ] {
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
    let scratch = Scratch::new("relay-key");
    let port = free_port();
    let token = cli(scratch.path(), &["admin-token"]).trim().to_owned();
    cli(
        scratch.path(),
        &[
            "add-user",
            "--first",
            "Ada",
            "--last",
            "Lovelace",
            "--email",
            "ada@example.com",
        ],
    );
    let minted = cli(
        scratch.path(),
        &["add-key", "--email", "ada@example.com", "--name", "laptop"],
    );
    // The CLI prints the secret somewhere in its output.
    let key = minted
        .split_whitespace()
        .find(|w| w.starts_with("tap_key_") || (w.starts_with("tap_") && *w != token))
        .unwrap_or_else(|| panic!("no key in the CLI output: {minted}"))
        .to_owned();

    let child = Command::new(BIN)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        .env("TAB_ATELIER_PROXY_CONFIG", scratch.path().join("config"))
        .env("TAB_ATELIER_PROXY_STATE", scratch.path().join("state"))
        .env("HOME", scratch.path().join("home"))
        .env(
            "TAB_ATELIER_PROXY_WEB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
        )
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut serving = Serving(child);
    wait_until_listening(port, &mut serving.0);

    // Presented as a browser credential, it must fail: the digest is computed
    // against the operator token, so a key simply does not produce a match.
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
}

/// Replaying a captured credential fails.
#[test]
fn a_captured_credential_cannot_be_replayed() {
    let (_serving, port, token) = start("replay");

    let challenge = get(port, "/api/users");
    let credential = credential(&token, "GET", "/api/users", &nonce_of(&challenge));
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
    let (_serving, port, token) = start("path-bound");

    let challenge = get(port, "/api/users");
    // Computed for `/`, then sent at `/api/users`.
    let credential = credential(&token, "GET", "/", &nonce_of(&challenge));
    let response = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status(&response), 401, "a credential for another path must not verify");
}

/// A credential made for another verb does not open this one.
#[test]
fn a_credential_made_for_a_get_does_not_authorize_a_delete() {
    let (_serving, port, token) = start("verb-bound");

    let challenge = get(port, "/api/users/nobody");
    let credential = credential(&token, "GET", "/api/users/nobody", &nonce_of(&challenge));
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
    let (_serving, port, token) = start("query-bound");

    let target = "/api/usage?window=24h";
    let response = signed_get(port, target, &token);
    assert_eq!(
        status(&response),
        200,
        "a credential hashing the whole target must be accepted"
    );

    // And one that hashed only the path must not be.
    let challenge = get(port, target);
    let path_only = credential(&token, "GET", "/api/usage", &nonce_of(&challenge));
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
    let (_serving, port, _token) = start("exempt");

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
    let (_serving, port, _token) = start("relay-refusal");
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
    let (_serving, port, _token) = start("preflight");
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
    let (serving, port, _token) = start("malformed");

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
    let _ = serving;
}

/// The wrong password is refused, and said so.
#[test]
fn the_wrong_password_is_refused() {
    let (_serving, port, _token) = start("wrong-password");

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
    let (_serving, port, _token) = start("json-refusal");
    let response = get(port, "/api/users");
    let body = body(&response);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("not JSON: {body}"));
    assert!(parsed.get("error").is_some(), "{parsed}");
}
