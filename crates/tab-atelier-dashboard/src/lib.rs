// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier-dashboard` — the web dashboard's own origin.
//!
//! # What lives here, and why
//!
//! The dashboard UI used to be served by the daemon itself, so a browser tab
//! pointed at the daemon had to reach every other endpoint the UI talks to
//! (catalog, decisions, tabs, streams) at the same origin — and when the UI was
//! hosted on its own port, the browser refused the cross-origin calls. This
//! crate sits in front of the daemon: it serves the UI assets it embeds,
//! answers the harness routes it owns, and reverse-proxies everything else to
//! the daemon on the same origin. The browser only ever sees one origin, so no
//! CORS, no second token, no mixed-content surprises.
//!
//! The daemon stays the only thing that knows about tabs; this crate depends on
//! its HTTP API alone (main's types are `pub(crate)` and deliberately not
//! imported here).
//!
//! # Routes
//!
//! - `/`, `/dashboard` — the dashboard page (embedded asset).
//! - `/assets/*` — the embedded UI assets (HTML/CSS/JS).
//! - anything else — reverse-proxied to the upstream daemon, method, headers,
//!   query, body and response stream included. That covers the harness routes
//!   the UI reads (`/decisions`, `/reports`, `/intent`, `/tabs/usage`, …):
//!   they are served by the daemon, not by this crate.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::combinators::BoxBody as BoxBodyInner;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;

/// The daemon this crate fronts when `TAB_ATELIER_UPSTREAM` is unset.
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:7890";

/// Address the dashboard listens on when `TAB_ATELIER_DASHBOARD_ADDR` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:7899";

/// Largest request body accepted before proxying, in bytes.
///
/// The proxied traffic is the UI's own JSON calls — a few KB each. Bounding it
/// keeps a client (or a bug) from making the dashboard buffer an arbitrary
/// amount of memory before the daemon ever sees the request. ponytail: this
/// also caps a future large upload (file share); raise it, or stream the body
/// through, when that lands.
const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

/// How many chunks may wait between the upstream reader and hyper.
const STREAM_CHANNEL_DEPTH: usize = 16;

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../assets/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../assets/dashboard.js");

/// A body that is either a complete in-memory buffer (local page, stub, error)
/// or a stream fed by the upstream hop (proxy). Both are `Send` and hyper-ready.
type BoxBody = BoxBodyInner<Bytes, std::io::Error>;

fn full(body: impl Into<Bytes>) -> BoxBody {
    Full::new(body.into()).map_err(|never| match never {}).boxed()
}

/// A JSON response carrying `value`, for everything this crate answers itself.
fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(value.to_string()))
        .expect("static response builder")
}

/// Where a request goes: a handful of paths are ours, the rest is the daemon's.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Serve an embedded asset: `(body, content-type)`.
    Asset(&'static str, &'static str),
    /// Everything else: hand it to the upstream daemon.
    Proxy,
}

fn route(req: &Request<()>) -> Route {
    // Only fetches can be ours. A POST/PUT/DELETE aimed at a local path is the
    // daemon's business, never a page fetch.
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Route::Proxy;
    }
    match req.uri().path() {
        "/" | "/dashboard" | "/assets/dashboard.html" => Route::Asset(DASHBOARD_HTML, "text/html; charset=utf-8"),
        "/assets/dashboard.css" => Route::Asset(DASHBOARD_CSS, "text/css; charset=utf-8"),
        "/assets/dashboard.js" => Route::Asset(DASHBOARD_JS, "text/javascript; charset=utf-8"),
        _ => Route::Proxy,
    }
}

/// Runtime knobs, read from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the daemon to proxy to, no trailing slash.
    pub upstream: String,
    /// Address to listen on.
    pub bind: SocketAddr,
}

impl Config {
    /// Read `TAB_ATELIER_UPSTREAM` and `TAB_ATELIER_DASHBOARD_ADDR`, falling
    /// back to [`DEFAULT_UPSTREAM`] and [`DEFAULT_BIND`].
    ///
    /// # Errors
    ///
    /// Fails when the upstream is not an http(s) URL or the bind address is
    /// unparseable.
    pub fn from_env() -> Result<Self, String> {
        let upstream = std::env::var("TAB_ATELIER_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
        let upstream = upstream.trim_end_matches('/').to_string();
        if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
            return Err(format!("TAB_ATELIER_UPSTREAM must be an http(s) URL, got {upstream:?}"));
        }
        let raw = std::env::var("TAB_ATELIER_DASHBOARD_ADDR").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        let bind = raw
            .parse()
            .map_err(|err| format!("TAB_ATELIER_DASHBOARD_ADDR {raw:?} is not an address: {err}"))?;
        Ok(Self { upstream, bind })
    }
}

/// Deadlines for the upstream hop, one per stage that must produce something.
///
/// Split out from [`Upstream`] so a test can compress them and still exercise
/// the real timeout paths instead of waiting out the production values.
#[derive(Debug, Clone, Copy)]
struct HopTimeouts {
    resolve: Option<Duration>,
    connect: Option<Duration>,
    send_request: Option<Duration>,
    send_body: Option<Duration>,
    /// Deadline for the RESPONSE HEADERS to arrive.
    ///
    /// `None` on purpose, and it must stay that way. ureq keeps this stage armed
    /// while the body is being read: its deadline is fixed at the moment the
    /// headers landed, so a non-`None` value caps the TOTAL life of the body,
    /// killing a healthy stream at a fixed age. Waiting for headers is still
    /// bounded — the `send_request` deadline covers that window.
    recv_response: Option<Duration>,
    /// Idle deadline on the response body: a stream that keeps producing
    /// survives however long it likes, one that goes quiet this long is dead.
    /// This, not `recv_response`, is what bounds a body.
    recv_body: Option<Duration>,
}

impl Default for HopTimeouts {
    fn default() -> Self {
        Self {
            resolve: Some(Duration::from_secs(5)),
            connect: Some(Duration::from_secs(10)),
            send_request: Some(Duration::from_secs(10)),
            send_body: Some(Duration::from_secs(30)),
            recv_response: None,
            recv_body: Some(Duration::from_mins(5)),
        }
    }
}

/// The daemon base URL plus the agent used to reach it.
///
/// One agent for the whole process: it owns the connection pool, and it is the
/// single place the hop's timeouts are configured.
#[derive(Debug)]
struct Upstream {
    base: String,
    agent: ureq::Agent,
}

impl Upstream {
    fn new(base: String, timeouts: HopTimeouts) -> Self {
        let agent = ureq::Agent::config_builder()
            // A 404 from the daemon is a 404 for the browser, not an upstream
            // failure to be rewritten into a 502.
            .http_status_as_error(false)
            // The browser follows redirects, and its cookie/authorization
            // handling must see the real target.
            .max_redirects(0)
            .timeout_resolve(timeouts.resolve)
            .timeout_connect(timeouts.connect)
            .timeout_send_request(timeouts.send_request)
            .timeout_send_body(timeouts.send_body)
            .timeout_recv_response(timeouts.recv_response)
            .timeout_recv_body(timeouts.recv_body)
            .build()
            .new_agent();
        Self { base, agent }
    }
}

/// Bind and serve until killed. See [`Config`] for the knobs.
///
/// # Errors
///
/// Fails when the listen address cannot be bound.
pub async fn run(cfg: Config) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    serve(listener, cfg.upstream).await
}

/// Serve on an already-bound listener until the process is killed. Split out
/// from [`run`] so a test can bind port 0 and learn the port it got.
///
/// # Errors
///
/// Does not return errors: a failed `accept` is retried and a failed connection
/// is dropped. The `Result` matches [`run`]'s shape for callers.
pub async fn serve(listener: tokio::net::TcpListener, upstream: String) -> std::io::Result<()> {
    serve_with(listener, upstream, HopTimeouts::default()).await
}

/// [`serve`] with explicit hop deadlines. Tests use it to compress the timeouts
/// so they exercise the real paths without waiting out the production values.
async fn serve_with(listener: tokio::net::TcpListener, upstream: String, timeouts: HopTimeouts) -> std::io::Result<()> {
    let upstream = Arc::new(Upstream::new(upstream, timeouts));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A single failed accept (fd exhaustion, client gone) must not take
            // the dashboard down with it; the next connection is unaffected.
            Err(err) => {
                eprintln!("accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let upstream = Arc::clone(&upstream);
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let upstream = Arc::clone(&upstream);
                async move { Ok::<_, std::convert::Infallible>(handle(req, &upstream).await) }
            });
            // Connection-level errors (client hung up mid-response) are routine.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(req: Request<Incoming>, upstream: &Upstream) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();
    match route(&Request::from_parts(parts.clone(), ())) {
        Route::Asset(asset, content_type) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            // The page is embedded in the binary and changes on deploy only, but
            // it names the assets by path: no-cache keeps a rebuilt deploy from
            // pairing a stale page with fresh JS.
            .header(header::CACHE_CONTROL, "no-cache")
            .body(full(asset))
            .expect("static response builder"),
        Route::Proxy => proxy(Request::from_parts(parts, body), upstream).await,
    }
}

/// Headers that describe THIS hop and must not be relayed to the next one.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            // ureq picks its own encoding and decodes it back; forwarding the
            // browser's preference would only invite a double decode.
            | "accept-encoding"
    )
}

async fn proxy(req: Request<Incoming>, upstream: &Upstream) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();

    // Bounded, buffered read: ureq wants the whole body up front anyway, and the
    // limit is what keeps a big upload from being accumulated in memory.
    let body = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) if err.is::<http_body_util::LengthLimitError>() => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &serde_json::json!({ "error": "request_body_too_large" }),
            );
        }
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "unreadable_request_body" }),
            );
        }
    };

    let target = format!(
        "{}{}{}",
        upstream.base,
        parts.uri.path(),
        parts.uri.query().map_or_else(String::new, |q| format!("?{q}"))
    );

    let method = parts.method.clone();
    let headers: Vec<(HeaderName, HeaderValue)> = parts
        .headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();

    let agent = upstream.agent.clone();
    // ureq is blocking; running it on the reactor would stall every other
    // connection for the duration of the hop.
    let upstream_resp = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let mut call = http::Request::builder().method(method).uri(&target);
        for (name, value) in &headers {
            call = call.header(name, value);
        }
        let request = call
            .body(body.to_vec())
            .map_err(|err| format!("bad upstream request: {err}"))?;
        agent.run(request).map_err(|err| format!("{err}"))
    })
    .await
    .map_err(|err| format!("upstream task: {err}"))
    .and_then(std::convert::identity);

    let (parts, body) = match upstream_resp {
        Ok(resp) => resp.into_parts(),
        Err(detail) => {
            // The reason may name an internal host: log it, tell the browser
            // only that the upstream is the one that failed.
            eprintln!("upstream hop failed: {detail}");
            return json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({ "error": "upstream_unavailable" }),
            );
        }
    };

    // Hand the upstream body to hyper as it arrives instead of buffering it:
    // that is what keeps a chunked/streamed response flowing. The blocking
    // reader owns the connection, so it runs on a thread of its own; it ends on
    // EOF, on any read error, on its idle timeout, or when `tx` fails because
    // the browser went away.
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(STREAM_CHANNEL_DEPTH);
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut reader = body.into_reader();
        let mut buf = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(Frame::data(Bytes::copy_from_slice(&buf[..n]))))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    break;
                }
            }
        }
    });

    let mut builder = Response::builder().status(parts.status);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(StreamBody::new(tokio_stream_of(rx)).boxed())
        .expect("proxied headers came from a real response")
}

/// Adapt the chunk channel into something [`StreamBody`] accepts.
fn tokio_stream_of(
    mut rx: mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::io::{Read, Write};

    use super::*;

    /// Factor applied to the production hop deadlines in tests that exercise a
    /// timeout path, so they run in well under a second instead of minutes.
    const SCALE: u32 = 100;

    /// Floor for the scaled idle deadline; see [`scaled`]. Must stay comfortably
    /// above [`DRIP_GAP`] that a scheduler hiccup cannot trip it.
    const MIN_IDLE_DEADLINE: Duration = Duration::from_secs(10);

    /// Drip scenario for the liveness test: this many chunks, this far apart.
    ///
    /// The gap is deliberately **over a second**. ureq applies its deadlines as
    /// socket timeouts, turning an already-elapsed one into a 1s floor rather
    /// than into "do not wait" (`NextTimeout::not_zero`). A sub-second gap is
    /// therefore masked by that floor and cannot tell an armed `recv_response`
    /// from an unset one — a drip paced faster than 1s passes either way, which
    /// is how an earlier version of this very test stayed blind to the bug it
    /// was written to catch.
    const DRIP_CHUNKS: usize = 3;
    const DRIP_GAP: Duration = Duration::from_millis(1_200);
    const DRIP_TOTAL: Duration = Duration::from_millis(3_600);

    /// Shrink every armed deadline by `factor`, leaving `None` as `None`.
    ///
    /// `recv_response`'s `None` is the point of the exercise: it must survive
    /// scaling untouched.
    ///
    /// The idle (`recv_body`) deadline gets a floor. Scaling it by the same
    /// factor would leave it barely above the drip gap, so a loaded machine
    /// stretching one sleep would turn the liveness test flaky for a reason
    /// that has nothing to do with what it tests. It only fires when the stream
    /// stalls, so keeping it generous costs no wall-clock time.
    fn scaled(mut timeouts: HopTimeouts, factor: u32) -> HopTimeouts {
        let shrink = |d: Option<Duration>| d.map(|d| d / factor);
        timeouts.resolve = shrink(timeouts.resolve);
        timeouts.connect = shrink(timeouts.connect);
        timeouts.send_request = shrink(timeouts.send_request);
        timeouts.send_body = shrink(timeouts.send_body);
        timeouts.recv_response = shrink(timeouts.recv_response);
        timeouts.recv_body = timeouts.recv_body.map(|d| (d / factor).max(MIN_IDLE_DEADLINE));
        timeouts
    }

    /// Read one request off a socket, stopping at the end of its head.
    fn read_request_head(stream: &mut impl Read) -> String {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while stream.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    /// Answer with a fixed body and close the connection.
    fn write_fixed_response(stream: &mut impl Write, body: &str) {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    }

    /// Blocking stand-in for the daemon: answers with the JSON-escaped request
    /// head it received, so a test can assert on the request line AND on the
    /// headers that survived (or did not survive) the hop.
    fn spawn_upstream() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let head = read_request_head(&mut stream);
                let quoted = serde_json::to_string(&head).unwrap();
                write_fixed_response(&mut stream, &format!(r#"{{"upstream_saw":{quoted}}}"#));
            }
        });
        port
    }

    /// Upstream that accepts and then never answers — the F1 hang.
    #[allow(
        clippy::collection_is_never_read,
        reason = "the sockets must stay open; dropping them would close the connection and turn the timeout into an EOF"
    )]
    fn spawn_silent_upstream() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                held.push(stream); // kept open, never written to
            }
        });
        port
    }

    /// Upstream that answers `Transfer-Encoding: chunked` and drips its body
    /// one chunk per `gap`, so a test can prove the response is relayed as it
    /// arrives rather than buffered, and that a slow-but-alive stream is not
    /// killed at a fixed age.
    fn spawn_dripping_upstream(chunks: usize, gap: Duration) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = read_request_head(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
                );
                for i in 0..chunks {
                    let chunk = format!("data: {i}\n\n");
                    let _ = write!(stream, "{:x}\r\n", chunk.len());
                    let _ = stream.write_all(chunk.as_bytes());
                    let _ = stream.write_all(b"\r\n");
                    let _ = stream.flush();
                    std::thread::sleep(gap);
                }
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
            }
        });
        port
    }

    /// Bind the dashboard on an ephemeral port and run it on its own runtime,
    /// with the production hop deadlines.
    fn spawn_dashboard(upstream_port: u16) -> u16 {
        spawn_dashboard_with(upstream_port, HopTimeouts::default())
    }

    fn spawn_dashboard_with(upstream_port: u16, timeouts: HopTimeouts) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let upstream = format!("http://127.0.0.1:{upstream_port}");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                serve_with(listener, upstream, timeouts).await.unwrap();
            });
        });
        port
    }

    fn request(port: u16, method: &str, path: &str, extra_headers: &[(&str, &str)]) -> (u16, String, String) {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut call = http::Request::builder()
            .method(method)
            .uri(format!("http://127.0.0.1:{port}{path}"));
        for (name, value) in extra_headers {
            call = call.header(*name, *value);
        }
        let resp = agent.run(call.body(Vec::new()).unwrap()).unwrap();
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, ctype, resp.into_body().read_to_string().unwrap())
    }

    fn get(port: u16, path: &str) -> (u16, String, String) {
        request(port, "GET", path, &[])
    }

    #[test]
    fn routes_are_classified_locally_or_proxied() {
        let r = |method: Method, path: &str| {
            let req = Request::builder().method(method).uri(path).body(()).unwrap();
            route(&req)
        };
        assert!(matches!(r(Method::GET, "/"), Route::Asset(..)));
        assert!(matches!(r(Method::GET, "/dashboard"), Route::Asset(..)));
        assert!(matches!(r(Method::GET, "/assets/dashboard.css"), Route::Asset(..)));
        // /reports is the daemon's (it lists the outbox markdown bundles).
        assert_eq!(r(Method::GET, "/reports"), Route::Proxy);
        // The daemon's own API is never ours, whatever the method.
        assert_eq!(r(Method::GET, "/api/tabs"), Route::Proxy);
        assert_eq!(r(Method::POST, "/api/tabs"), Route::Proxy);
        // A POST to a local path is not a page fetch: it must reach the daemon.
        assert_eq!(r(Method::POST, "/dashboard"), Route::Proxy);
    }

    #[test]
    fn serves_the_embedded_assets_locally() {
        let upstream = spawn_upstream();
        let port = spawn_dashboard(upstream);

        let (status, ctype, body) = get(port, "/assets/dashboard.css");
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/css; charset=utf-8");
        assert!(!body.is_empty(), "the embedded stylesheet must not be empty");

        let (status, ctype, body) = get(port, "/");
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/html; charset=utf-8");
        assert!(body.contains("<html"), "the page must be the real HTML");

        let (status, ctype, body) = get(port, "/assets/dashboard.js");
        assert_eq!((status, ctype.as_str()), (200, "text/javascript; charset=utf-8"));
        assert!(body.contains("export"), "the module must be the real script");
    }

    #[test]
    fn proxies_path_headers_and_strips_hop_by_hop() {
        let upstream = spawn_upstream();
        let port = spawn_dashboard(upstream);

        let (status, ctype, body) = request(
            port,
            "GET",
            "/api/tabs?limit=2",
            &[("X-Test", "forwarded"), ("Proxy-Authorization", "Bearer do-not-relay")],
        );
        assert_eq!(status, 200);
        assert_eq!(ctype, "application/json");

        let echoed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let seen = echoed["upstream_saw"]
            .as_str()
            .expect("upstream echoes the request head");
        let seen_lower = seen.to_lowercase();

        // (a) path and query reach the daemon verbatim
        assert!(seen.starts_with("GET /api/tabs?limit=2 "), "request line: {seen}");
        // (b) an ordinary header is forwarded
        assert!(seen_lower.contains("x-test: forwarded"), "custom header lost: {seen}");
        // (c) a hop-by-hop header is not
        assert!(
            !seen_lower.contains("proxy-authorization"),
            "hop-by-hop header relayed: {seen}"
        );
        // (d) Host is rewritten to the upstream authority, never the dashboard's
        assert!(
            seen_lower.contains(&format!("host: 127.0.0.1:{upstream}")),
            "Host must be the upstream's: {seen}"
        );
        assert!(
            !seen_lower.contains(&format!("host: 127.0.0.1:{port}")),
            "the dashboard's own authority leaked upstream: {seen}"
        );
    }

    #[test]
    fn relays_a_streamed_response_whole() {
        let upstream = spawn_dripping_upstream(2, Duration::from_millis(50));
        let port = spawn_dashboard(upstream);

        let (status, ctype, body) = get(port, "/api/stream");
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/event-stream");
        assert_eq!(body, "data: 0\n\ndata: 1\n\n");
    }

    /// A slow-but-alive stream must outlive every deadline that stays armed
    /// while its body is being read.
    ///
    /// The deadlines are scaled down by [`SCALE`] so the test is fast. The point
    /// is the shape of the config, not its absolute numbers: with
    /// `recv_response` unset the stream survives however long it keeps
    /// producing; arm it with ANY value and ureq kills this drip at that value,
    /// because its deadline is fixed when the headers land.
    #[test]
    fn a_slow_but_alive_stream_outlives_the_other_deadlines() {
        let timeouts = scaled(HopTimeouts::default(), SCALE);
        // Preconditions, so a later tweak to the scenario cannot quietly make
        // this test vacuous.
        for armed in [timeouts.send_request, timeouts.send_body, timeouts.recv_response]
            .into_iter()
            .flatten()
        {
            assert!(
                armed < DRIP_TOTAL,
                "the drip ({DRIP_TOTAL:?}) must outlast the {armed:?} deadline to prove anything"
            );
        }
        assert!(
            DRIP_GAP < timeouts.recv_body.unwrap(),
            "a gap between chunks must stay under the idle deadline"
        );

        let upstream = spawn_dripping_upstream(DRIP_CHUNKS, DRIP_GAP);
        let port = spawn_dashboard_with(upstream, timeouts);

        let (status, ctype, body) = get(port, "/api/stream");
        assert_eq!((status, ctype.as_str()), (200, "text/event-stream"), "body: {body}");
        let expected: String = (0..DRIP_CHUNKS).fold(String::new(), |mut acc, i| {
            write!(acc, "data: {i}\n\n").unwrap();
            acc
        });
        assert_eq!(body, expected, "the slow stream was cut short");
    }

    /// `recv_response` must stay unset: it is the one stage ureq leaves armed
    /// while reading the body, so giving it a value caps a stream's total life.
    #[test]
    fn the_response_deadline_is_not_armed() {
        assert!(
            HopTimeouts::default().recv_response.is_none(),
            "arming recv_response kills a healthy long-lived stream at a fixed age"
        );
    }

    #[test]
    fn an_oversized_body_is_rejected_before_reaching_the_daemon() {
        let upstream = spawn_upstream();
        let port = spawn_dashboard(upstream);

        let oversized = vec![b'x'; MAX_REQUEST_BODY + 1];
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let resp = agent
            .post(format!("http://127.0.0.1:{port}/api/echo"))
            .send(&oversized[..])
            .unwrap();
        assert_eq!(resp.status().as_u16(), 413);
        let body = resp.into_body().read_to_string().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "request_body_too_large"
        );
    }

    #[test]
    fn a_silent_upstream_answers_502_instead_of_hanging() {
        let upstream = spawn_silent_upstream();
        // Compressed deadlines, so the test does not wait out the production
        // values to prove the same code path.
        let port = spawn_dashboard_with(upstream, scaled(HopTimeouts::default(), SCALE));

        let started = std::time::Instant::now();
        let (status, ctype, body) = get(port, "/api/tabs");
        let elapsed = started.elapsed();

        assert_eq!((status, ctype.as_str()), (502, "application/json"), "body: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "upstream_unavailable"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the 502 must come from a deadline, took {elapsed:?}"
        );
        // The internal detail must not reach the client.
        assert!(!body.contains("timeout"), "upstream error detail leaked: {body}");
    }
}
