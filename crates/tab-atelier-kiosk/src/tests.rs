// SPDX-License-Identifier: MPL-2.0

//! Where the Kiosk's HTTP behaviour is pinned.
//!
//! Unit tests rather than integration tests, because the things worth checking
//! are not the public API: the socket loop, the hop deadlines and the
//! classification table are all `pub(crate)`. What is checked is what a CLIENT
//! meets, so each test stands up real loopback sockets and drives them — a test
//! that only calls the pieces would not see a status line built wrong or a
//! header dropped somewhere between them.

use std::fs;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http::{Method, Request, header};
use tokio::net::TcpListener;

use crate::config::{Config, HopTimeouts};
use crate::decisions::Journal;
use crate::server::serve_with;

/// Factor the production deadlines are compressed by in these tests.
///
/// The deadlines exist to bound a remote that has stopped answering; keeping
/// them at their real values would make a test that trips one wait minutes.
const SCALE: u32 = 100;

/// Floor under a scaled deadline, so none becomes too tight to measure.
const MIN_DEADLINE: Duration = Duration::from_millis(100);

fn scaled(base: Duration) -> Duration {
    (base / SCALE).max(MIN_DEADLINE)
}

fn scaled_timeouts() -> HopTimeouts {
    let base = HopTimeouts::default();
    HopTimeouts {
        resolve: base.resolve.map(scaled),
        connect: base.connect.map(scaled),
        send_request: base.send_request.map(scaled),
        send_body: base.send_body.map(scaled),
        // Stays `None` however it is scaled: see the dedicated test below.
        recv_response: base.recv_response.map(scaled),
        recv_body: base.recv_body.map(scaled),
    }
}

/// A fresh state directory, with an empty outbox, for one test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ta-kiosk-tests-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("outbox")).expect("make outbox");
    dir
}

/// A config with the journal and remotes under `state`.
fn test_config(state: &Path, token: Option<&str>) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().expect("a valid bind address"),
        journal: Journal {
            path: state.join("decisions.jsonl"),
            outbox: state.join("outbox"),
        },
        token: token.map(str::to_string),
        token_file: None,
        remotes: crate::remotes::Remotes {
            path: state.join("remotes.json"),
        },
        intentions: crate::intent::Intentions::new(state.join("intentions")).expect("intentions dir"),
        // The UI is served from disk, so the tests point at the crate's own
        // assets — the same files the packaged install ships.
        web_root: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"),
    }
}

/// Bind the kiosk on an ephemeral port and return where it landed.
async fn spawn_kiosk_with(cfg: Config) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind kiosk");
    let addr = listener.local_addr().expect("kiosk address");
    tokio::spawn(async move {
        let _ = serve_with(listener, cfg, scaled_timeouts()).await;
    });
    addr
}

/// A kiosk with its own uniquely-named state directory, no token.
async fn spawn_kiosk() -> SocketAddr {
    // A unique state dir per call: several tests run concurrently and each
    // `scratch_dir` wipes its directory, so a shared name would have one test
    // deleting another's state mid-run.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let name = format!("kiosk{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    spawn_kiosk_with(test_config(&scratch_dir(&name), None)).await
}

fn agent() -> ureq::Agent {
    // A 4xx/5xx is a status to assert on, not a transport error to raise.
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// One request through a fresh agent (so a test never reuses a pooled socket).
fn send(
    method: Method,
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> http::Response<ureq::Body> {
    let mut builder = Request::builder().method(method).uri(format!("http://{addr}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    agent()
        .run(builder.body(body).expect("build request"))
        .expect("kiosk answered")
}

fn get(addr: SocketAddr, path: &str) -> http::Response<ureq::Body> {
    send(Method::GET, addr, path, &[], Vec::new())
}

fn body_string(mut resp: http::Response<ureq::Body>) -> String {
    let mut out = String::new();
    resp.body_mut().as_reader().read_to_string(&mut out).expect("read body");
    out
}

/// The page and its assets come from this crate, with their content types, and
/// no daemon is needed to serve them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_the_embedded_assets_locally() {
    // Port 1: nothing is there, so anything proxied fails. The local fetches
    // must still answer, or the page would not render without a daemon.
    let addr = spawn_kiosk().await;

    let page = get(addr, "/");
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(body_string(page).to_ascii_lowercase().contains("<!doctype html"));

    let script = get(addr, "/assets/kiosk.js");
    assert_eq!(script.status(), 200);
    assert!(
        script.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(script.headers().contains_key(header::CACHE_CONTROL));
    assert!(body_string(script).len() > 1000);
}

/// The `recv_response` deadline must stay unarmed. ureq keeps that deadline
/// armed while the body is being read, so any value caps a stream's TOTAL life;
/// a body is bounded by the idle `recv_body` deadline instead.
#[test]
fn the_response_deadline_is_not_armed() {
    assert!(
        HopTimeouts::default().recv_response.is_none(),
        "recv_response must stay None or it would cap every streamed body's life"
    );
}

/// Encode a path for a query, the way a browser would.
fn urlencode(raw: &str) -> String {
    raw.replace('/', "%2F")
}

/// The board is behind the token, and a kiosk with no token lets nobody in.
///
/// The pane is the kiosk's own, so a refused request is a 401 here and never a
/// relay to the daemon — proving the guard is the kiosk's, not the daemon's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_decision_pane_requires_the_token() {
    let dir = scratch_dir("pane-auth");
    let addr = spawn_kiosk_with(test_config(&dir, Some("s3cret"))).await;

    let anon = get(addr, "/decisions");
    assert_eq!(anon.status(), 401, "no token is refused");

    let wrong = send(
        Method::GET,
        addr,
        "/decisions",
        &[("authorization", "Bearer nope")],
        vec![],
    );
    assert_eq!(wrong.status(), 401, "a wrong token is refused");

    let ok = send(
        Method::GET,
        addr,
        "/decisions",
        &[("authorization", "Bearer s3cret")],
        vec![],
    );
    assert_eq!(ok.status(), 200);
    assert!(body_string(ok).contains("\"decisions\""));

    // A kiosk with no token configured refuses everyone, not no one.
    let open_dir = scratch_dir("pane-notoken");
    let open = spawn_kiosk_with(test_config(&open_dir, None)).await;
    assert_eq!(get(open, "/decisions").status(), 401);
}

/// The whole board round-trips over HTTP: list, read, rule, serve a linked file,
/// and refuse everything that should be refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_board_is_read_and_ruled_through_http() {
    let dir = scratch_dir("pane-board");
    fs::write(
        dir.join("decisions.jsonl"),
        "{\"id\":\"a\",\"title\":\"Ship it?\"}\n{\"id\":\"b\",\"state\":\"archived\",\"title\":\"Old\"}\n",
    )
    .expect("write journal");
    fs::write(dir.join("outbox/note.md"), "# note\n").expect("write note");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    // Open listed, archived hidden until asked for.
    let board = body_string(send(Method::GET, addr, "/decisions", &[auth], vec![]));
    assert!(board.contains("\"id\":\"a\""), "open decision listed: {board}");
    assert!(!board.contains("\"id\":\"b\""), "archived hidden by default");
    let all = body_string(send(
        Method::GET,
        addr,
        "/decisions?includeArchived=true",
        &[auth],
        vec![],
    ));
    assert!(all.contains("\"id\":\"b\""), "archived shown when asked");

    // A linked file is served from inside the outbox with its type...
    let note = send(
        Method::GET,
        addr,
        &format!(
            "/decisions/file?path={}",
            urlencode(&dir.join("outbox/note.md").to_string_lossy())
        ),
        &[auth],
        vec![],
    );
    assert_eq!(note.status(), 200);
    assert_eq!(body_string(note), "# note\n");

    // ... and a real file OUTSIDE the outbox is refused, so the guarantee is the
    // confinement and not the file's existence.
    let outside = send(
        Method::GET,
        addr,
        &format!(
            "/decisions/file?path={}",
            urlencode(&dir.join("decisions.jsonl").to_string_lossy())
        ),
        &[auth],
        vec![],
    );
    assert_eq!(outside.status(), 404, "a real file outside the outbox is still refused");

    // Read, then rule.
    let read = send(Method::POST, addr, "/decisions/a/read", &[auth], vec![]);
    assert_eq!(read.status(), 200);
    assert!(body_string(read).contains("\"state\":\"read\""));

    let ruled = send(
        Method::POST,
        addr,
        "/decisions/a/tranch",
        &[auth],
        br#"{"verdict":"ship"}"#.to_vec(),
    );
    assert_eq!(ruled.status(), 200);
    let ruled_body = body_string(ruled);
    assert!(ruled_body.contains("\"state\":\"tranched\""));
    assert!(ruled_body.contains("\"verdict\":\"ship\""));

    // An unknown id is a 404; a body that is not the expected JSON is a 400.
    let unknown = send(
        Method::POST,
        addr,
        "/decisions/zzz/tranch",
        &[auth],
        br#"{"verdict":"x"}"#.to_vec(),
    );
    assert_eq!(unknown.status(), 404);
    let junk = send(Method::POST, addr, "/decisions/a/tranch", &[auth], b"not json".to_vec());
    assert_eq!(junk.status(), 400);
}

/// The reports tab lists the outbox, newest first, and is behind the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reports_pane_lists_the_outbox() {
    let dir = scratch_dir("reports");
    fs::write(dir.join("outbox/audit-01.md"), "x").expect("write report");
    fs::write(dir.join("outbox/notes.txt"), "x").expect("write a non-report");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(get(addr, "/reports").status(), 401, "no token is refused");

    let listed = send(Method::GET, addr, "/reports", &[auth], vec![]);
    assert_eq!(listed.status(), 200);
    let body = body_string(listed);
    assert!(body.contains("\"reports\""), "the listing envelope: {body}");
    assert!(body.contains("audit-01.md"), "the markdown report is listed: {body}");
    assert!(!body.contains("notes.txt"), "a non-markdown file is not a report");
    assert!(body.contains("outbox/audit-01.md"), "path is how the page addresses it");
}

/// The intent tab records an intention and works it: list, view, ask, promote —
/// all behind the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_intent_pane_records_and_works_an_intention() {
    let dir = scratch_dir("intent");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(
        send(Method::POST, addr, "/intent", &[], br#"{"content":"x"}"#.to_vec()).status(),
        401,
        "no token is refused"
    );

    // Record one; its first line titles it, and the title becomes the slug.
    let recorded = send(
        Method::POST,
        addr,
        "/intent",
        &[auth],
        r#"{"content":"Décision congés\n\nThe team needs dates.\n"}"#.as_bytes().to_vec(),
    );
    assert_eq!(recorded.status(), 201, "a new intention is a 201");
    let body = body_string(recorded);
    assert!(
        body.contains("\"slug\":\"decision-conges\""),
        "slugged from the title: {body}"
    );

    // The list shows it; the view returns the whole file.
    let listed = body_string(send(Method::GET, addr, "/intent/list", &[auth], vec![]));
    assert!(listed.contains("decision-conges"), "the list has it: {listed}");
    let view = send(Method::GET, addr, "/intent/decision-conges", &[auth], vec![]);
    assert_eq!(view.status(), 200);
    assert!(
        body_string(view).contains("The team needs dates."),
        "the pitch is there"
    );

    // Ask appends a turn to the same file.
    let asked = send(
        Method::POST,
        addr,
        "/intent/decision-conges/ask",
        &[auth],
        br#"{"text":"what dates?"}"#.to_vec(),
    );
    assert_eq!(asked.status(), 200);
    assert!(body_string(asked).contains("### Question"), "the turn is a heading");

    // Promote moves it to the ready pool — a move, not a flag.
    let promoted = send(Method::POST, addr, "/intent/decision-conges/promote", &[auth], vec![]);
    assert_eq!(promoted.status(), 200);
    assert!(
        dir.join("intentions/READY-intentions/decision-conges.md").is_file(),
        "promote moves the file into the ready pool"
    );

    // An empty intention is a 400; an unknown slug a 404.
    assert_eq!(
        send(Method::POST, addr, "/intent", &[auth], br#"{"content":"   "}"#.to_vec()).status(),
        400
    );
    assert_eq!(send(Method::GET, addr, "/intent/nope", &[auth], vec![]).status(), 404);
}

/// The catalog tab loads: the Kiosk serves the read-model rather than relaying
/// to a daemon that has no such route — where the daemon's auth gate would
/// answer 401, reading as "your token is wrong" for a route that is not there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_catalog_pane_serves_the_read_model() {
    let dir = scratch_dir("catalog");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(get(addr, "/catalog/list").status(), 401, "still behind the token");

    let listed = send(Method::GET, addr, "/catalog/list", &[auth], vec![]);
    assert_eq!(listed.status(), 200);
    let body = body_string(listed);
    assert!(body.contains("\"retired\""), "the read-model the page expects: {body}");
    assert!(body.contains("\"skills\""), "the read-model the page expects: {body}");
}

/// A Kiosk path reached with the wrong method is a 405 with an `Allow` header,
/// not a relay to the daemon's misleading 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_method_on_a_pane_is_a_405_not_a_relay() {
    let dir = scratch_dir("405");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;

    // No token needed: a method error is not data, so it is answered before the
    // gate — and never handed to a daemon that is not listening here.
    let resp = send(Method::GET, addr, "/intent", &[], vec![]);
    assert_eq!(resp.status(), 405);
    assert_eq!(
        resp.headers()[header::ALLOW].to_str().unwrap(),
        "POST",
        "the Allow header names what the route does answer"
    );
}

/// The fleet view — the landing page's data — is behind the token and, with no
/// remotes configured, is an empty list rather than an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fleet_lists_every_server_behind_the_token() {
    let dir = scratch_dir("fleet");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(get(addr, "/kiosk/fleet").status(), 401, "the fleet is behind the token");

    let listed = send(Method::GET, addr, "/kiosk/fleet", &[auth], vec![]);
    assert_eq!(listed.status(), 200);
    let body = body_string(listed);
    assert!(body.contains("\"servers\""), "the fleet envelope: {body}");
}

/// The servers can be added, listed and forgotten through the Kiosk's own API —
/// and the remote's token never comes back to the browser. The Kiosk is what
/// holds it, so it is the Kiosk, not a page, that reaches the fleet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remotes_are_managed_server_side() {
    let dir = scratch_dir("remotes");
    let addr = spawn_kiosk_with(test_config(&dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(
        get(addr, "/kiosk/remotes").status(),
        401,
        "the remote list is behind the token"
    );

    // Add a server. Point it at a closed port so the later reach fails fast.
    let added = send(
        Method::POST,
        addr,
        "/kiosk/remotes",
        &[auth],
        br#"{"label":"build-box","url":"http://127.0.0.1:1","token":"remote-secret"}"#.to_vec(),
    );
    assert_eq!(added.status(), 201);
    let added_body = body_string(added);
    assert!(
        added_body.contains("build-box"),
        "the added server comes back: {added_body}"
    );
    assert!(
        !added_body.contains("remote-secret"),
        "the remote's token must not be echoed: {added_body}"
    );
    let added_json: serde_json::Value = serde_json::from_str(&added_body).expect("the reply is JSON");
    let id = added_json["remote"]["id"]
        .as_str()
        .expect("the new remote has an id")
        .to_string();

    // The list redacts the token; the file on disk holds it.
    let listed = body_string(send(Method::GET, addr, "/kiosk/remotes", &[auth], vec![]));
    assert!(listed.contains("build-box"));
    assert!(
        listed.contains("\"has_token\":true"),
        "the list says a token is set: {listed}"
    );
    assert!(!listed.contains("remote-secret"), "the list never carries the token");
    let on_disk = fs::read_to_string(dir.join("remotes.json")).expect("read remotes");
    assert!(on_disk.contains("remote-secret"), "the token IS stored, server-side");

    // A malformed add is a 400.
    let bad = send(
        Method::POST,
        addr,
        "/kiosk/remotes",
        &[auth],
        br#"{"label":"","url":"not-a-url"}"#.to_vec(),
    );
    assert_eq!(bad.status(), 400);

    // Reaching the server fails closed: a 502, naming nothing internal.
    let unreachable = send(Method::GET, addr, &format!("/kiosk/remotes/{id}/tabs"), &[auth], vec![]);
    assert_eq!(unreachable.status(), 502, "a down remote is a 502, not a hang");
    assert!(body_string(unreachable).contains("remote_unreachable"));

    // Forget it.
    let removed = send(
        Method::POST,
        addr,
        &format!("/kiosk/remotes/{id}/remove"),
        &[auth],
        vec![],
    );
    assert_eq!(removed.status(), 200);
    let after = body_string(send(Method::GET, addr, "/kiosk/remotes", &[auth], vec![]));
    assert!(!after.contains("build-box"), "the server is gone: {after}");
}
