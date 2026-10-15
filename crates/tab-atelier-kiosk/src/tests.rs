// SPDX-License-Identifier: MPL-2.0

//! Where the Kiosk's HTTP behaviour is pinned.
//!
//! Unit tests rather than integration tests, because the things worth checking
//! are not the public API: the socket loop, the hop deadlines and the
//! classification table are all `pub(crate)`. What is checked is what a CLIENT
//! meets, so each test stands up real loopback sockets and drives them — a test
//! that only calls the pieces would not see a status line built wrong or a
//! header dropped somewhere between them.

use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener as StdListener, TcpStream as StdStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use http::{Method, Request, header};
use tokio::net::TcpListener;

use crate::config::{Config, HopTimeouts};
use crate::decisions::Journal;
use crate::proxy::MAX_REQUEST_BODY;
use crate::server::serve_with;

/// Factor the production deadlines are compressed by in these tests.
///
/// The deadlines exist to bound a peer that has stopped answering; keeping them
/// at their real values would make every test that trips one wait minutes. The
/// compression is uniform, so relative timing (a live stream outliving the
/// others) still means what it means in production.
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

/// A config pointed at `upstream`, with the journal under `state`.
fn test_config(upstream: String, state: &Path, token: Option<&str>) -> Config {
    Config {
        upstream,
        bind: "127.0.0.1:0".parse().expect("a valid bind address"),
        journal: Journal {
            path: state.join("decisions.jsonl"),
            outbox: state.join("outbox"),
        },
        token: token.map(str::to_string),
        token_file: None,
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

async fn spawn_kiosk(upstream: String) -> SocketAddr {
    spawn_kiosk_with(test_config(upstream, &scratch_dir("proxy"), None)).await
}

/// Reads a request head (through the blank line) off a blocking socket.
fn read_head(sock: &mut StdStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match sock.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// A stub "daemon": accepts one connection, echoes `{head}\n--\n{body}` back to
/// the test, and answers with a fixed status, headers and body.
fn spawn_upstream(
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static [u8],
) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind upstream");
    let addr = listener.local_addr().expect("upstream address");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let head = read_head(&mut sock);
        let mut request_body = vec![0_u8; content_length(&head)];
        let _ = sock.read_exact(&mut request_body);
        let _ = tx.send(format!("{head}\n--\n{}", String::from_utf8_lossy(&request_body)));

        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (name, value) in headers {
            let _ = write!(out, "{name}: {value}\r\n");
        }
        let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        let _ = sock.write_all(&bytes);
        let _ = sock.flush();
    });
    (addr, rx)
}

/// A stub "daemon" that answers 200 and drips four 4-byte chunks, one per
/// `gap`, then ends the body — a stand-in for a streamed tab or an SSE feed.
fn spawn_dripping_upstream(chunks: usize, gap: Duration) -> SocketAddr {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind upstream");
    let addr = listener.local_addr().expect("upstream address");
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let _ = read_head(&mut sock);
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n");
        for index in 0..chunks {
            std::thread::sleep(gap);
            let data = format!("{index:04}");
            let _ = sock.write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes());
        }
        let _ = sock.write_all(b"0\r\n\r\n");
        let _ = sock.flush();
    });
    addr
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
    let addr = spawn_kiosk("http://127.0.0.1:1".to_string()).await;

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

/// A relayed request reaches the daemon with its path, method and body intact,
/// its custom headers kept, and the hop-by-hop headers that describe the
/// browser<->kiosk hop stripped so the daemon<->kiosk hop is described honestly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxies_a_request_and_strips_hop_by_hop_headers() {
    let (upstream, seen) = spawn_upstream("200 OK", vec![("content-type", "text/plain")], b"pong");
    let addr = spawn_kiosk(format!("http://{upstream}")).await;

    let resp = send(
        Method::POST,
        addr,
        "/decisions",
        &[
            ("x-probe", "sentinel"),
            ("connection", "keep-alive"),
            ("te", "trailers"),
        ],
        b"payload".to_vec(),
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(body_string(resp), "pong");

    let relayed = seen
        .recv_timeout(Duration::from_secs(2))
        .expect("upstream saw the request");
    let head = relayed.to_lowercase();
    assert!(
        relayed.starts_with("POST /decisions "),
        "path and method forwarded: {relayed}"
    );
    assert!(head.contains("x-probe: sentinel"), "custom header kept");
    assert!(
        !head.contains("\nconnection:"),
        "connection is hop-by-hop and must not be relayed"
    );
    assert!(!head.contains("\nte:"), "te is hop-by-hop and must not be relayed");
    assert!(relayed.contains("payload"), "request body forwarded");
}

/// A streamed upstream answer arrives whole, chunk by chunk, and the client
/// sees the reassembled body. This is the path a live tab or an SSE feed takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relays_a_streamed_response_whole() {
    let upstream = spawn_dripping_upstream(4, Duration::from_millis(20));
    let addr = spawn_kiosk(format!("http://{upstream}")).await;
    let body = body_string(get(addr, "/tabs"));
    assert_eq!(body, "0000000100020003", "all four chunks, in order, whole");
}

/// A stream that stays alive past where a fixed total-life deadline would have
/// cut it still finishes. This is what the unarmed `recv_response` buys: a
/// timeout armed there would cap the whole body's life and kill a healthy feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_but_alive_stream_is_not_cut_short() {
    let gap = scaled(HopTimeouts::default().recv_body.expect("recv_body is set")) / 8;
    let upstream = spawn_dripping_upstream(8, gap);
    let addr = spawn_kiosk(format!("http://{upstream}")).await;
    let body = body_string(get(addr, "/tabs"));
    assert_eq!(body.len(), 8 * 4, "every chunk arrived despite the pace");
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

/// A body over the relay limit is refused at the kiosk, before the daemon is
/// asked to hold it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_body_is_rejected_before_the_daemon() {
    let (upstream, seen) = spawn_upstream("200 OK", vec![], b"pong");
    let addr = spawn_kiosk(format!("http://{upstream}")).await;

    let oversized = vec![b'x'; MAX_REQUEST_BODY + 1];
    let resp = send(Method::POST, addr, "/api/upload", &[], oversized);
    assert_eq!(resp.status(), 413);

    // The daemon must never have been reached: the refusal happens here.
    assert!(
        seen.recv_timeout(Duration::from_millis(300)).is_err(),
        "the daemon was contacted despite the early refusal"
    );
}

/// An upstream that cannot be reached is a 502 for the browser, not a hang and
/// not the local page.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_upstream_answers_502() {
    let addr = spawn_kiosk("http://127.0.0.1:1".to_string()).await;
    // A path the kiosk does not own, so it is relayed — and the daemon is absent.
    let resp = get(addr, "/tabs");
    assert_eq!(resp.status(), 502);
    let body = body_string(resp);
    assert!(
        body.contains("upstream_unavailable"),
        "the reason is named, not leaked: {body}"
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
    let addr = spawn_kiosk_with(test_config("http://127.0.0.1:1".to_string(), &dir, Some("s3cret"))).await;

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
    let open = spawn_kiosk_with(test_config("http://127.0.0.1:1".to_string(), &open_dir, None)).await;
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
    let addr = spawn_kiosk_with(test_config("http://127.0.0.1:1".to_string(), &dir, Some("t"))).await;
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
    let addr = spawn_kiosk_with(test_config("http://127.0.0.1:1".to_string(), &dir, Some("t"))).await;
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

/// The intent tab records an intention as a server-named file, and refuses an
/// empty one — both behind the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_intent_pane_records_an_intention() {
    let dir = scratch_dir("intent");
    let addr = spawn_kiosk_with(test_config("http://127.0.0.1:1".to_string(), &dir, Some("t"))).await;
    let auth = ("authorization", "Bearer t");

    assert_eq!(
        send(Method::POST, addr, "/intent", &[], br#"{"content":"x"}"#.to_vec()).status(),
        401,
        "no token is refused"
    );

    let recorded = send(
        Method::POST,
        addr,
        "/intent",
        &[auth],
        br##"{"content":"# Intention\n\nDo the thing.\n"}"##.to_vec(),
    );
    assert_eq!(recorded.status(), 201, "a new artefact is a 201");
    let body = body_string(recorded);
    assert!(body.contains("\"name\":\"intent-"), "server-named: {body}");

    // It is really on disk, and it is a report the other tab now lists.
    let written = fs::read_dir(dir.join("outbox"))
        .expect("read outbox")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("intent-"))
        .count();
    assert_eq!(written, 1, "exactly one artefact was written");
    assert!(body_string(send(Method::GET, addr, "/reports", &[auth], vec![])).contains("intent-"));

    // An empty intention is a 400, and posts nothing.
    let empty = send(Method::POST, addr, "/intent", &[auth], br#"{"content":"   "}"#.to_vec());
    assert_eq!(empty.status(), 400);
    let junk = send(Method::POST, addr, "/intent", &[auth], b"not json".to_vec());
    assert_eq!(junk.status(), 400);
}
