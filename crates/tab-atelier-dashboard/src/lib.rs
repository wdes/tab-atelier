// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier-dashboard` — the web dashboard's own origin.
//!
//! # What lives here, and why
//!
//! The dashboard UI used to be served by the daemon itself, so a browser tab
//! pointed at the daemon had to reach every other endpoint the UI talks to
//! (catalog, decisions, tabs, SSE streams) at the same origin — and when the UI
//! was hosted on its own port, the browser refused the cross-origin calls.
//! This crate sits in front of the daemon: it serves the UI assets it embeds,
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
//! - `/dashboard/state`, `/dashboard/activity`, `/dashboard/share-token`,
//!   `/reports` — harness routes owned by this crate. They are STUBS today: the
//!   real payloads are derived read-models built from daemon internals that are
//!   not on the HTTP API yet, so they answer `501` with a JSON body naming the
//!   route rather than pretending. TODO: back them with daemon endpoints.
//! - anything else — reverse-proxied to the upstream daemon, method, headers,
//!   query, body and response stream included.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;

/// The daemon this crate fronts when `TAB_ATELIER_UPSTREAM` is unset.
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:7890";

/// Address the dashboard listens on when `TAB_ATELIER_DASHBOARD_ADDR` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:7899";

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../assets/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../assets/dashboard.js");

/// A body that is either a complete in-memory buffer (local page, stub) or a
/// stream fed by the upstream hop (proxy). Both are `Send` and hyper-ready.
type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full(body: impl Into<Bytes>) -> BoxBody {
    Full::new(body.into()).map_err(|never| match never {}).boxed()
}

/// Where a request goes: a handful of paths are ours, the rest is the daemon's.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Serve an embedded asset: `(body, content-type)`.
    Asset(&'static str, &'static str),
    /// A harness route this crate owns but does not implement yet.
    Stub,
    /// Everything else: hand it to the upstream daemon.
    Proxy,
}

fn route(req: &Request<()>) -> Route {
    let path = req.uri().path();
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Route::Proxy;
    }
    match path {
        "/" | "/dashboard" | "/assets/dashboard.html" => Route::Asset(DASHBOARD_HTML, "text/html; charset=utf-8"),
        "/dashboard/state" | "/dashboard/activity" | "/dashboard/share-token" | "/reports" => Route::Stub,
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
    /// Fails when the bind address is set but unparseable.
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
/// Does not return errors: a failed `accept` is retried, a failed connection is
/// dropped. The `Result` is there to match [`run`]'s shape for callers.
pub async fn serve(listener: tokio::net::TcpListener, upstream: String) -> std::io::Result<()> {
    let upstream = Arc::new(upstream);
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
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let upstream = Arc::clone(&upstream);
                async move { handle(req, &upstream).await }
            });
            // A client that hung up mid-response is routine, not worth a log line.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
}

async fn handle(req: Request<hyper::body::Incoming>, upstream: &str) -> Result<Response<BoxBody>, std::io::Error> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    match route(&Request::from_parts(parts.clone(), ())) {
        Route::Asset(asset, content_type) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            // The page is embedded in the binary and changes on deploy only, but
            // it names the assets by path: no-cache keeps a rebuilt deploy from
            // pairing a stale page with fresh JS.
            .header(header::CACHE_CONTROL, "no-cache")
            .body(full(asset))
            .expect("static response builder")),
        Route::Stub => {
            let body = format!(
                r#"{{"error":"not_implemented","route":"{path}","detail":"harness route owned by tab-atelier-dashboard; not backed by the daemon API yet"}}"#
            );
            Ok(Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .header(header::CONTENT_TYPE, "application/json")
                .body(full(body))
                .expect("static response builder"))
        }
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

async fn proxy(req: Request<hyper::body::Incoming>, upstream: &str) -> Result<Response<BoxBody>, std::io::Error> {
    let (parts, body) = req.into_parts();
    // Buffered, not streamed: these are JSON/command calls of a few KB, and
    // ureq needs the whole body up front anyway. ponytail: a large upload
    // (file share) would be held in memory — stream it if that lands.
    let body = body.collect().await.map_err(std::io::Error::other)?.to_bytes();

    let target = format!(
        "{upstream}{}{}",
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

    // ureq is blocking; running it on the reactor would stall every other
    // connection for the duration of the hop.
    let result = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let agent = ureq::Agent::config_builder()
            // A 404 from the daemon is a 404 for the browser, not an upstream
            // failure to be rewritten into a 502.
            .http_status_as_error(false)
            // The browser follows redirects, and its cookie/authorization
            // handling must see the real target.
            .max_redirects(0)
            .build()
            .new_agent();
        let mut call = http::Request::builder().method(method).uri(&target);
        for (name, value) in &headers {
            call = call.header(name, value);
        }
        let request = call
            .body(body.to_vec())
            .map_err(|err| format!("bad upstream request: {err}"))?;
        agent.run(request).map_err(|err| format!("upstream hop failed: {err}"))
    })
    .await
    .map_err(std::io::Error::other)?
    .map_err(std::io::Error::other)?;

    let (parts, body) = result.into_parts();

    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
    // The upstream body is a blocking reader (this is what keeps SSE flowing
    // instead of being buffered until close). A dedicated thread pulls it and
    // hands chunks to hyper; the `tx` drop when the client disconnects ends it.
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
    Ok(builder
        .body(StreamBody::new(tokio_stream_of(rx)).boxed())
        .expect("proxied headers came from a real response"))
}

/// Adapt the chunk channel into something [`StreamBody`] accepts.
fn tokio_stream_of(
    mut rx: mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

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

    /// Answer with a fixed body and close.
    fn write_fixed_response(stream: &mut impl Write, body: &str) {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    }

    /// Minimal blocking stand-in for the daemon: echoes back the request line it
    /// received, so a test can prove the path survived the hop.
    fn spawn_upstream() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let head = read_request_head(&mut stream);
                let line = head.lines().next().unwrap_or_default().to_string();
                write_fixed_response(&mut stream, &format!(r#"{{"upstream_saw":"{line}"}}"#));
            }
        });
        port
    }

    /// Bind the dashboard on an ephemeral port and run it on its own runtime.
    fn spawn_dashboard(upstream_port: u16) -> u16 {
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
                serve(listener, upstream).await.unwrap();
            });
        });
        port
    }

    fn get(port: u16, path: &str) -> (u16, String, String) {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let resp = agent.get(format!("http://127.0.0.1:{port}{path}")).call().unwrap();
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, ctype, resp.into_body().read_to_string().unwrap())
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
        assert_eq!(r(Method::GET, "/dashboard/state"), Route::Stub);
        assert_eq!(r(Method::GET, "/reports"), Route::Stub);
        // The daemon's own API is never ours, whatever the method.
        assert_eq!(r(Method::GET, "/api/tabs"), Route::Proxy);
        assert_eq!(r(Method::POST, "/api/tabs"), Route::Proxy);
        // A POST to a local path is not a page fetch: it must reach the daemon.
        assert_eq!(r(Method::POST, "/dashboard"), Route::Proxy);
    }

    #[test]
    fn serves_embedded_assets_and_stubs_locally() {
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

        let (status, ctype, body) = get(port, "/dashboard/state");
        assert_eq!(status, 501);
        assert_eq!(ctype, "application/json");
        assert!(
            body.contains("/dashboard/state"),
            "the stub must name its route: {body}"
        );
    }

    #[test]
    fn proxies_everything_else_to_upstream() {
        let upstream = spawn_upstream();
        let port = spawn_dashboard(upstream);

        let (status, ctype, body) = get(port, "/api/tabs?limit=2");
        assert_eq!(status, 200);
        assert_eq!(ctype, "application/json");
        assert!(
            body.contains(r#""upstream_saw":"GET /api/tabs?limit=2 HTTP/1.1""#),
            "path and query must reach the daemon verbatim: {body}"
        );
    }
}
