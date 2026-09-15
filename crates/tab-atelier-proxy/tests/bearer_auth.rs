// SPDX-License-Identifier: MPL-2.0

//! The auth boundary, against the real binary over a real socket.
//!
//! Unit tests can prove that a token comparison is constant-time; only this can
//! prove the thing that matters — that an authenticated caller is served and an
//! anonymous one is not, and that the paths which have to stay reachable still
//! are.
//!
//! It runs `tab-atelier-proxy` as a process for the same reason
//! `end_to_end.rs` does: the guards, the catchers and the route table only meet
//! each other inside a running server, and whether a refusal reaches the wire
//! with the right status and the right sentence is not visible from a library
//! call.
//!
//! The page and its assets are deliberately NOT behind the credential — see
//! [`the_page_loads_without_a_credential`] for why, and for what was tried
//! before that.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The binary under test, built by cargo for this integration target.
const BIN: &str = env!("CARGO_BIN_EXE_tab-atelier-proxy");

/// A directory that cleans itself up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("ta-bearer-auth-{name}-{}", std::process::id()));
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
/// Returning them detached from the child is how the first version of this file
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

/// A GET presenting an operator token the way a client would.
fn get_authed(port: u16, path: &str, token: &str) -> String {
    http(
        port,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             Connection: close\r\n\r\n"
        ),
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

// ── the decision this file pins ──────────────────────────────────────────────

/// The page and its assets load for anyone; nothing in them is secret.
///
/// This is a deliberate reversal. The proxy briefly served the dashboard behind
/// an HTTP Digest challenge, which gated the HTML, the CSS and the scripts along
/// with it. It was removed because it protected nothing: the page is a shell
/// with no figures in it, every value on screen comes from `/api/*`, and each of
/// those needs the operator token. What the gate did buy was a second credential
/// — the browser held a digest while the page asked for the same token again —
/// and a class of failure where being signed in did not mean the page was.
#[test]
fn the_page_loads_without_a_credential() {
    let proxy = Proxy::start("open-page");
    let port = proxy.port;

    for path in ["/", "/index.html", "/app.js", "/api.js", "/charts.js"] {
        let response = get(port, path);
        assert_eq!(status(&response), 200, "{path} should load for anyone");
    }

    // And the page really is the shell: the form is in it, and account data is
    // not, because the data was never in the file.
    let page = body(&get(port, "/"));
    assert!(page.contains("Sign in"), "the form is part of the page");
    assert!(!page.contains("ada@example.com"), "and no account data is");
}

/// And the API refuses an anonymous request, naming what is missing.
#[test]
fn the_api_refuses_an_anonymous_request() {
    let proxy = Proxy::start("anon-api");
    let port = proxy.port;

    for path in [
        "/api/users",
        "/api/providers",
        "/api/pressure",
        "/api/usage",
        "/api/inspect",
    ] {
        let response = get(port, path);
        assert_eq!(status(&response), 401, "{path} must need the token");
        assert!(
            body(&response).contains("admin token"),
            "the refusal should name what is missing: {}",
            body(&response)
        );
    }
}

/// A token that is not the token is refused.
#[test]
fn a_wrong_token_is_refused() {
    let proxy = Proxy::start("wrong-token");
    let port = proxy.port;

    for wrong in ["tap_nope", "", " ", &proxy.token.to_uppercase()] {
        let response = get_authed(port, "/api/users", wrong);
        assert_eq!(status(&response), 401, "{wrong:?} was accepted: {}", body(&response));
    }
    // The real one still works, so the loop above is testing the comparison
    // rather than a server that refuses everything.
    assert_eq!(status(&get_authed(port, "/api/users", &proxy.token)), 200);
}

/// The operator token opens the API, in either spelling the server reads.
#[test]
fn the_operator_token_opens_the_api() {
    let proxy = Proxy::start("bearer");
    let port = proxy.port;

    for header in [
        format!("Authorization: Bearer {}", proxy.token),
        format!("x-api-key: {}", proxy.token),
        // The scheme is case-insensitive in HTTP, and clients differ.
        format!("Authorization: bearer {}", proxy.token),
    ] {
        let response = http(
            port,
            &format!("GET /api/users HTTP/1.1\r\nHost: x\r\n{header}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status(&response), 200, "{header} was refused");
        assert!(body(&response).contains("\"users\""), "{}", body(&response));
    }
}

/// A user's relay key is not an operator credential.
///
/// The two are minted by different commands and are not interchangeable. If a
/// relay key opened `/api/*`, every tab could rewrite every account.
#[test]
fn a_relay_key_does_not_open_the_operator_surface() {
    let scratch = Scratch::new("relay-key");
    cli(scratch.path(), &["add", "Ada", "Lovelace", "ada@example.com"]);
    let minted = cli(scratch.path(), &["add-key", "ada@example.com", "laptop"]);
    let proxy = Proxy::start_with(scratch);
    let port = proxy.port;

    let key = minted
        .lines()
        .find_map(|l| l.trim().strip_prefix("key: "))
        .unwrap_or_else(|| panic!("no key in the CLI output: {minted}"))
        .trim()
        .to_owned();
    assert!(key.starts_with("tap_"), "unexpected key: {key}");
    assert_ne!(key, proxy.token, "the fixture must use two different secrets");

    let response = get_authed(port, "/api/users", &key);
    assert_eq!(status(&response), 401, "a relay key must not administer the proxy");

    // And the same key DOES work at the relay, which is what it is for. Without
    // this the assertion above would pass even if the key were simply invalid.
    // The check is on the proxy's own wording rather than on the status: this
    // goes on to a provider, and a provider is entitled to answer 401 —
    // Anthropic does, for the fake token this fixture uses.
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

/// The relay needs its own key even though the page does not.
///
/// The two rules are independent: the page is open and the API is not.
#[test]
fn a_relay_request_without_a_key_is_refused() {
    let proxy = Proxy::start("relay-anon");
    let port = proxy.port;

    let anonymous = http(
        port,
        "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\n\
         Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status(&anonymous), 401);
    assert!(body(&anonymous).contains("no valid key"), "{}", body(&anonymous));

    // The operator token is not a relay key either: a different secret for a
    // different job.
    let as_operator = http(
        port,
        &format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            proxy.token
        ),
    );
    assert_eq!(
        status(&as_operator),
        401,
        "the operator token must not relay: {}",
        body(&as_operator)
    );
}

// ── what the page needs to work at all ───────────────────────────────────────

/// Every asset the browser asks for is served, including the source map.
///
/// The map is the one that bit: Bootstrap's CSS ends in
/// `sourceMappingURL=bootstrap.min.css.map`, so it is requested as soon as the
/// style sheet loads, and it was the one file the resolver did not know about.
///
/// Split in two, and the split is the point. `OURS` must be served everywhere,
/// because the package carries them. `THE_DISTRIBUTIONS` are resolved to
/// `/usr/share/javascript/bootstrap5/`, which is a runtime dependency of the
/// `.deb` and **absent from a bare CI runner** — so requiring them
/// unconditionally asserts that whoever runs the tests happens to have the
/// Debian package installed. That is not a property of this code, and this test
/// failed in CI for exactly that reason before it was split.
///
/// Absent-because-not-installed is therefore a skip, not a failure; present is
/// checked, so a wrong path in the resolver is still caught on any machine that
/// has the package. The count only guards against the check going vacuous.
#[test]
fn every_asset_the_page_needs_is_served() {
    /// Files the package carries, so they must be served anywhere.
    const OURS: [&str; 8] = [
        "/",
        "/index.html",
        "/app.js",
        "/api.js",
        "/charts.js",
        "/vendor/vue.global.prod.js",
        "/robots.txt",
        "/favicon.ico",
    ];
    /// Resolved to `/usr/share/javascript/bootstrap5/`, which is a runtime
    /// dependency of the `.deb` and absent from a bare CI runner.
    const THE_DISTRIBUTIONS: [&str; 2] = ["/vendor/bootstrap.min.css", "/vendor/bootstrap.min.css.map"];

    let proxy = Proxy::start("assets");
    let port = proxy.port;

    for path in OURS {
        let response = get(port, path);
        assert_eq!(status(&response), 200, "{path} is missing");
        assert!(!body(&response).is_empty(), "{path} is empty");
    }

    let mut checked = 0;
    for path in THE_DISTRIBUTIONS {
        let response = get(port, path);
        match status(&response) {
            200 => {
                assert!(!body(&response).is_empty(), "{path} is empty");
                checked += 1;
            }
            // The distribution's package is not installed here. Nothing this
            // crate controls can change that, and the `.deb` declares the
            // dependency, so an installed system has it.
            404 => {}
            code => panic!("{path} answered {code}, which is neither served nor absent"),
        }
    }

    // On a machine with the package — CI, once `libjs-bootstrap5` is listed,
    // or any host running the installed `.deb` — both are verified.
    if std::path::Path::new("/usr/share/javascript/bootstrap5").exists() {
        assert_eq!(checked, 2, "the distribution's assets are installed but not served");
    }
}

/// Each asset comes back with a type the browser will act on.
///
/// A script served as `application/json` is refused outright when the response
/// says not to sniff, and the page renders blank with one console line.
#[test]
fn each_asset_is_served_with_the_type_a_browser_needs() {
    let proxy = Proxy::start("content-types");
    let port = proxy.port;

    for (path, want) in [
        ("/", "text/html"),
        ("/app.js", "application/javascript"),
        ("/api.js", "application/javascript"),
        ("/charts.js", "application/javascript"),
        ("/vendor/vue.global.prod.js", "application/javascript"),
        ("/robots.txt", "text/plain"),
        ("/favicon.ico", "image/x-icon"),
    ] {
        let response = get(port, path);
        let got = header(&response, "content-type").unwrap_or_else(|| panic!("{path} answered with no content type"));
        assert!(got.starts_with(want), "{path} was served as {got}");
        assert_ne!(got, "application/json", "{path} was served as JSON");
    }
}

/// The page's script tags are files that exist, in an order that works, and
/// each carries a version.
///
/// `api.js` defines the client, `charts.js` the charts, `app.js` uses both — and
/// these are plain scripts sharing one scope rather than modules, so the order
/// is load-bearing. A file named in the page and absent from the package is a
/// blank dashboard, which is exactly what shipped once.
///
/// The `?v=` is the cache-buster the server adds. It is checked here rather than
/// assumed because a versioned name is the only thing that stops a browser
/// reusing a cached client against a newer server — and for `api.js` in
/// particular, a stale copy is a client naming routes that no longer exist.
#[test]
fn the_page_names_scripts_that_are_all_served_and_versioned() {
    let proxy = Proxy::start("script-order");
    let port = proxy.port;
    let page = body(&get(port, "/"));

    let mut seen = Vec::new();
    for part in page.split("<script src=\"").skip(1) {
        let src = part.split('"').next().expect("a src attribute").to_owned();
        let (name, version) = src
            .split_once("?v=")
            .unwrap_or_else(|| panic!("{src} is not versioned, so a browser may serve a cached copy"));
        assert!(!version.is_empty(), "{src} has an empty version");

        assert_eq!(
            status(&get(port, &format!("/{name}"))),
            200,
            "the page names {name}, which is not served"
        );
        seen.push(name.to_owned());
    }

    assert_eq!(
        seen,
        vec![
            "vendor/vue.global.prod.js".to_owned(),
            "api.js".to_owned(),
            "charts.js".to_owned(),
            "app.js".to_owned(),
        ],
        "Vue first, then the client, the charts, and the app that uses them"
    );
}

// ── what stays open, and why ─────────────────────────────────────────────────

/// The crawler files and the probe need no credential.
#[test]
fn the_crawler_files_and_the_probe_are_open() {
    let proxy = Proxy::start("open-extras");
    let port = proxy.port;

    for path in ["/robots.txt", "/favicon.ico", "/api/hello"] {
        assert_eq!(status(&get(port, path)), 200, "{path} should be open");
    }
    // The probe answers from memory, with a two-byte JSON object rather than an
    // empty string, so a client that parses what it gets is not handed nothing
    // and left reporting a syntax error instead of "the process is up".
    assert_eq!(
        body(&get(port, "/api/hello")),
        "{}",
        "the probe answers 200 with an empty JSON object"
    );
}

#[test]
fn a_preflight_is_answered_without_a_credential() {
    let proxy = Proxy::start("preflight");
    let port = proxy.port;

    let response = http(
        port,
        "OPTIONS /api/users HTTP/1.1\r\nHost: x\r\n\
         Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), 204);
    let allowed = header(&response, "access-control-allow-headers").expect("the header list");
    for needed in ["authorization", "x-api-key", "content-type"] {
        assert!(allowed.contains(needed), "{needed} is not allowed: {allowed}");
    }
}

// ── refusals are answerable ──────────────────────────────────────────────────

/// A malformed header is refused, not fatal.
///
/// `Authorization` arrives off the wire before anything has validated it.
/// Slicing it at a fixed byte index panics when that index lands inside a
/// multi-byte character, which is a 500 on a path nobody has authenticated on.
/// That was a real bug; the scheme test now uses `strip_prefix`, which returns
/// `None` instead.
#[test]
fn a_malformed_authorization_header_is_refused_rather_than_fatal() {
    let proxy = Proxy::start("malformed");
    let port = proxy.port;

    let mut bad_headers = vec![
        "Authorization:".to_owned(),
        "Authorization: Bearer".to_owned(),
        "Authorization: Bearer ".to_owned(),
        "Authorization: Basic dXNlcjpwdw==".to_owned(),
        "Authorization: Digest username=\"a\"".to_owned(),
        format!("Authorization: {}", "ü".repeat(20)),
        format!("Authorization: Bearer {}", "ü".repeat(20)),
    ];
    // Every prefix short enough to be sliced inside a multi-byte character, and
    // every one of them used to be a byte index the old code could land on.
    for n in 0..=12 {
        bad_headers.push(format!("Authorization: Bearer {}", "ü".repeat(n)));
        bad_headers.push(format!("Authorization: {}", "ü".repeat(n)));
    }

    for header in bad_headers {
        let response = http(
            port,
            &format!("GET /api/users HTTP/1.1\r\nHost: x\r\n{header}\r\nConnection: close\r\n\r\n"),
        );
        let code = status(&response);
        assert!(
            code == 401 || code == 400,
            "{header:?} produced {code}, not a refusal: {}",
            &response[..response.len().min(200)]
        );
    }

    // And the server is still there.
    assert_eq!(status(&get(port, "/api/hello")), 200, "the process survived");
}

/// A refusal is JSON, with the reason in it.
///
/// The reason is the point. A browser hides a response body behind its auth
/// dialog, so a sentence that only lives in the body is a sentence nobody reads;
/// the catcher also logs it, and this pins the body side.
#[test]
fn a_refusal_is_json_with_a_reason() {
    let proxy = Proxy::start("json-refusal");
    let port = proxy.port;

    let response = get(port, "/api/users");
    assert_eq!(status(&response), 401);
    let text = body(&response);
    assert!(text.starts_with('{'), "not JSON: {text}");

    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let error = value["error"].as_str().expect("an error field");
    assert!(!error.is_empty(), "the reason must be readable");
    // The one thing an operator reading it needs.
    assert!(error.contains("admin token"), "{error}");
}
