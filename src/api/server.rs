// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP server plumbing for the local API: the hyper/TLS accept loop
//! (`start_api_server[_tls]`, `serve_connection`), the relay egress proxy
//! (`handle_relay`), the `MemAdapter` that bridges the sync `handle_connection`
//! into hyper, and the TLS cert bootstrap. Relocated VERBATIM from `api/mod.rs`
//! (Slice 4, pure move). Only adaptation: `use super::*` for the parent scope.

#[allow(clippy::wildcard_imports)]
use super::*;

// Async I/O — hyper drives connection setup, ALPN negotiation
// (h2/http/1.1) and keep-alive; the sync `handle_connection`
// handler runs unmodified per request via spawn_blocking against a
// `MemAdapter` (Cursor reader + Vec writer). Each persistent
// connection thus amortises TCP+TLS setup across every keystroke
// POST and every output poll — the change the user could feel.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};

/// Unified response body for the hyper service: most routes buffer a
/// `Full<Bytes>`, but the relay route streams SSE — both erase to this boxed
/// body (error type `Infallible`; an upstream read error just ends the stream).
type RespBody = BoxBody<Bytes, std::convert::Infallible>;
use hyper::server::conn::http1 as h1_conn;
use hyper::server::conn::http2 as h2_conn;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::convert::Infallible;
use tokio::net::TcpListener as TokioListener;

/// In-memory adapter that lets the existing sync handler read a
/// pre-formatted HTTP/1.1 request and write its response into a
/// `Vec<u8>` we can hand back to hyper. The input is the header block
/// CHAINED with hyper's collected body `Bytes` — the body used to be
/// appended into the header buffer, which duplicated every upload
/// (100 MiB cap, 3 in flight per token ⇒ hundreds of MiB of transient
/// RSS for data hyper already held).
struct MemAdapter {
    input: std::io::Chain<std::io::Cursor<Vec<u8>>, std::io::Cursor<Bytes>>,
    output: Vec<u8>,
}
impl Read for MemAdapter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}
impl Write for MemAdapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Format a hyper `Request` (already-collected body) as raw HTTP/1.1
/// bytes the existing handler can parse. The handler reads method +
/// path from the request line, headers (Authorization, Content-Length,
/// Accept-Encoding, If-None-Match), and then a body of `Content-Length`
/// bytes — everything else hyper sent is dropped.
pub(in crate::api) fn format_h1_request(
    method: &str,
    uri: &str,
    headers: &hyper::HeaderMap,
    body_len: usize,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    let _ = write!(&mut buf, "{method} {uri} HTTP/1.1\r\n");
    for (name, value) in headers {
        if name == hyper::header::CONTENT_LENGTH {
            // Force a length consistent with the actual body we ship.
            continue;
        }
        if let Ok(v) = value.to_str() {
            let _ = write!(&mut buf, "{}: {}\r\n", name.as_str(), v);
        }
    }
    let _ = write!(&mut buf, "Content-Length: {body_len}\r\n\r\n");
    buf
}

/// Parse the bytes emitted by `handle_connection` and return a hyper response.
///
/// The handler always emits `HTTP/1.1 STATUS REASON` + headers + body.
/// We ignore the reason phrase (hyper rebuilds it) and pass headers +
/// body through.
fn parse_h1_response(bytes: Vec<u8>) -> Response<Full<Bytes>> {
    let (status, headers, body_bytes) = parse_h1_parts(bytes);
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(body_bytes))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Pure core of [`parse_h1_response`]: (status, headers, body) parsed
/// out of the handler's raw bytes, with the body sliced zero-copy and
/// clamped to `Content-Length` when present.
pub(in crate::api) fn parse_h1_parts(bytes: Vec<u8>) -> (u16, Vec<(String, String)>, Bytes) {
    // Find header/body split.
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    // Move the handler's Vec into `Bytes` and slice the body out of it —
    // zero-copy, where this used to `copy_from_slice` the whole body
    // (up to a full file download) once more per request.
    let all = Bytes::from(bytes);
    let (head, body) = split.map_or_else(|| (all.clone(), Bytes::new()), |i| (all.slice(..i), all.slice(i + 4..)));
    let head_text = std::str::from_utf8(&head).unwrap_or("");
    let mut lines = head_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| {
            let mut parts = l.split_whitespace();
            parts.next(); // HTTP/1.1
            parts.next()
        })
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
            }
            headers.push((name.to_string(), value.to_string()));
        }
    }
    let body_bytes = content_length.map_or_else(|| body.clone(), |n| body.slice(..n.min(body.len())));
    (status, headers, body_bytes)
}

/// hyper service: collects the body, hands the request to the sync
/// handler on the blocking pool, parses the response back.
async fn handle_hyper_request(
    req: Request<Incoming>,
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
) -> Result<Response<RespBody>, Infallible> {
    let path = req.uri().path().to_string();
    // Intercept WS upgrade BEFORE we collect the body into the sync
    // adapter — the WS handshake needs the original Request so it
    // can return a 101 Switching Protocols + park the connection.
    if let Some((key, is_uuid)) = crate::api_ws::parse_ws_path(&path) {
        let key = key.to_string();
        return Ok(crate::api_ws::handle_upgrade(req, state, &token, read_only, key, is_uuid).map(BodyExt::boxed));
    }
    // Anthropic API relay (streaming SSE) — also handled natively so the
    // response body can stream, escaping the buffered `Full<Bytes>` path (like
    // the WS upgrade above). Everything else falls through to the sync handler.
    if path.starts_with("/relay/anthropic/") {
        return Ok(handle_relay(req, &token).await);
    }
    let method = req.method().to_string();
    let uri = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().to_string(), std::string::ToString::to_string);
    // Split the request instead of cloning the whole HeaderMap just
    // because `into_body()` would consume it.
    let (parts, body) = req.into_parts();
    let headers = parts.headers;
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(400)
                .body(Full::new(Bytes::from("bad body")).boxed())
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed())));
        }
    };
    let head = format_h1_request(&method, &uri, &headers, body.len());
    let resp = tokio::task::spawn_blocking(move || {
        let mut adapter = MemAdapter {
            // Chain the header block with hyper's body `Bytes` instead of
            // concatenating — no second copy of the (up to 100 MiB) body.
            input: std::io::Read::chain(std::io::Cursor::new(head), std::io::Cursor::new(body)),
            output: Vec::with_capacity(1024),
        };
        handle_connection(&mut adapter, &state, &token, read_only);
        adapter.output
    })
    .await
    .unwrap_or_default();
    Ok(parse_h1_response(resp).map(BodyExt::boxed))
}

/// A small buffered relay response (errors / 401s), boxed to match [`RespBody`].
fn relay_status(code: u16, msg: &str) -> Response<RespBody> {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Streaming Anthropic API relay (`/relay/anthropic/*`).
///
/// A native async handler so the SSE response streams end-to-end (the buffered
/// sync path can't). Role is config-driven: the **egress** instance forwards to
/// `api.anthropic.com` injecting the remote's Claude OAuth token (see
/// [`crate::relay`]); otherwise the **local** instance forwards to the
/// configured remote's `/relay/anthropic/*`. Auth: the local hop presents the
/// stand-in `x-api-key`, the egress hop a `Bearer` — both must equal this
/// instance's master token.
async fn handle_relay(req: Request<Incoming>, master_token: &str) -> Response<RespBody> {
    let method = req.method().clone();
    let full = req.uri().path();
    let sub = full.strip_prefix("/relay/anthropic").unwrap_or("").to_string();
    let sub_pq = req.uri().query().map_or_else(|| sub.clone(), |q| format!("{sub}?{q}"));
    let egress = crate::relay_egress();

    // Auth against this instance's master token (constant-time).
    let provided = if egress {
        req.headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned()
    } else {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    if !constant_time_eq(provided.as_bytes(), master_token.as_bytes()) {
        return relay_status(401, "relay: unauthorized");
    }

    let content_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let is_post = method == hyper::Method::POST;
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return relay_status(400, "relay: bad request body"),
    };
    let target = crate::relay_target();

    // Bridge ureq's blocking response reader → an async hyper stream. The
    // blocking task sends the (status, content-type) meta over a oneshot, then
    // pumps body chunks over an mpsc; the async side builds a StreamBody.
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<Result<(u16, Option<String>), String>>();
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::task::spawn_blocking(move || {
        let agent = crate::relay::relay_agent();
        let (url, bearer, cf) = if egress {
            let token = match crate::relay::oauth_access_token() {
                Ok(t) => t,
                Err(e) => {
                    let _ = meta_tx.send(Err(format!("egress oauth: {e}")));
                    return;
                }
            };
            (format!("{}{sub_pq}", crate::relay::upstream()), token, None)
        } else if let Some(t) = target {
            let cf = (!t.cf_access_client_id.is_empty())
                .then(|| (t.cf_access_client_id.clone(), t.cf_access_client_secret.clone()));
            (format!("{}/relay/anthropic{sub_pq}", t.url), t.token, cf)
        } else {
            let _ = meta_tx.send(Err("relay not configured (set relay_endpoint_id)".to_owned()));
            return;
        };

        // Build the header list once (ureq's POST/GET builders are distinct
        // typestates, so apply them inside each branch).
        let mut hdrs: Vec<(&str, String)> = vec![
            ("Content-Type", content_type),
            ("Authorization", format!("Bearer {bearer}")),
        ];
        if egress {
            hdrs.push(("anthropic-version", crate::relay::ANTHROPIC_VERSION.to_owned()));
            hdrs.push(("anthropic-beta", crate::relay::ANTHROPIC_BETA.to_owned()));
        } else {
            hdrs.push(("Accept", "application/json".to_owned()));
            if let Some((id, sec)) = cf {
                hdrs.push(("CF-Access-Client-Id", id));
                hdrs.push(("CF-Access-Client-Secret", sec));
            }
        }
        let sent = if is_post {
            let mut rb = agent.post(&url);
            for (k, v) in &hdrs {
                rb = rb.header(*k, v);
            }
            rb.send(&body[..])
        } else {
            let mut rb = agent.get(&url);
            for (k, v) in &hdrs {
                rb = rb.header(*k, v);
            }
            rb.call()
        };
        let mut resp = match sent {
            Ok(r) => r,
            Err(e) => {
                let _ = meta_tx.send(Err(format!("upstream: {e}")));
                return;
            }
        };
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if meta_tx.send(Ok((status, ctype))).is_err() {
            return;
        }
        let mut reader = resp.body_mut().as_reader();
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break, // EOF or upstream read error → end stream
                Ok(n) => {
                    if body_tx.blocking_send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                        break; // client hung up
                    }
                }
            }
        }
    });

    let meta = match meta_rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return relay_status(502, &format!("relay: {e}")),
        Err(_) => return relay_status(502, "relay: forward task died"),
    };
    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|b| (Ok::<_, std::convert::Infallible>(Frame::data(b)), rx))
    });
    let mut builder = Response::builder().status(meta.0);
    if let Some(ct) = meta.1 {
        builder = builder.header("content-type", ct);
    }
    builder
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Pick the right hyper connection driver for the negotiated ALPN.
/// Called from both the plain (no ALPN, default to h1) and TLS
/// (ALPN-negotiated) listener paths.
pub(in crate::api) async fn serve_connection<I>(
    io: I,
    h2: bool,
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
) where
    I: hyper::rt::Read + hyper::rt::Write + Send + Unpin + 'static,
{
    let svc = service_fn(move |req| handle_hyper_request(req, state.clone(), token.clone(), read_only));
    if h2 {
        let _ = h2_conn::Builder::new(TokioExecutor::new())
            .serve_connection(io, svc)
            .await;
    } else {
        // `.with_upgrades()` is what makes hyper relinquish the
        // socket to whatever awaits `hyper::upgrade::on(req)` (us,
        // for the WS handshake in api_ws). Without it, hyper closes
        // the connection the instant the 101 response is written
        // — handshake succeeds at the HTTP layer, then the socket
        // dies before the WS frame loop can take over. The client
        // sees `close 1006 <empty>` right after `open`.
        let _ = h1_conn::Builder::new()
            .keep_alive(true)
            // Slow-loris guard: bound how long a client may take to
            // dribble in its request headers. Without it a connection
            // that sends one byte every few seconds ties up a task
            // indefinitely, and the accept loop spawns an unbounded
            // task per connection. WS upgrades complete their headers
            // well within this window before handing off the socket.
            // `header_read_timeout` requires a timer to be installed,
            // else hyper panics when it arms the deadline.
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(io, svc)
            .with_upgrades()
            .await;
    }
}

/// Poll the global `SHUTDOWN_REQUESTED` and trigger the supplied
/// `Notify` when it flips. Used by both listeners to break out of
/// their accept loops on SIGTERM so the runtime can return, the
/// listening socket can be dropped, and the next daemon instance
/// can rebind without "Address already in use".
async fn shutdown_watcher(notify: Arc<tokio::sync::Notify>) {
    use std::sync::atomic::Ordering;
    loop {
        if crate::SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            notify.notify_waiters();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub fn start_api_server(state: Arc<Mutex<TabSnapshot>>, token: String, read_only: bool, bind: String) {
    // Publish the master token onto the shared snapshot the auth gate
    // reads, BEFORE any connection is served, so it's live-swappable via
    // POST /master-token/reset without a restart.
    if let Ok(mut s) = state.lock() {
        s.master_token.clone_from(&token);
    }
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("API: tokio runtime build failed: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match TokioListener::bind(&bind).await {
                Ok(l) => {
                    info!("API: listening on {bind} (HTTP/1.1)");
                    l
                }
                Err(e) => {
                    error!("API: failed to bind {bind}: {e}");
                    return;
                }
            };
            let shutdown = Arc::new(tokio::sync::Notify::new());
            tokio::spawn(shutdown_watcher(shutdown.clone()));
            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((stream, _)) = res else { continue };
                        let state = state.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            // Plain HTTP: no ALPN, HTTP/1.1 with
                            // keep-alive. HTTP/2 only over TLS.
                            serve_connection(TokioIo::new(stream), false, state, token, read_only).await;
                        });
                    }
                    () = shutdown.notified() => {
                        info!("API: SIGTERM received, closing :{bind} listener");
                        break;
                    }
                }
            }
            // Listener drops here, freeing the port for the next
            // process. In-flight connections finish on their own
            // tokio::spawn'd tasks before the runtime shuts down.
        });
    });
}

/// TLS listener — ALPN advertises `h2` and `http/1.1`, so modern
/// browsers negotiate HTTP/2 and we get multiplexing + persistent
/// connection for free over the share-link viewer.
///
/// `external_cert` is `Some((cert_path, key_path))` to serve a user-
/// supplied PEM cert + key (Cloudflare Origin, Let's Encrypt copy,
/// etc.) instead of the self-signed `tls.crt` in the state dir. Both
/// paths must be set; a half-configured pair is rejected at the call
/// site (in headless.rs / app.rs).
// `external_cert` + `client_ca` take owned `PathBuf`s rather than refs
// so the caller can fire-and-forget (this function spawns its own
// thread).
#[allow(clippy::needless_pass_by_value)]
pub fn start_api_server_tls(
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
    bind: String,
    external_cert: Option<(std::path::PathBuf, std::path::PathBuf)>,
    client_ca: Option<std::path::PathBuf>,
) {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::server::WebPkiClientVerifier;

    // Same as start_api_server: publish the master token onto the shared
    // snapshot before serving, so it's live-swappable.
    if let Ok(mut s) = state.lock() {
        s.master_token.clone_from(&token);
    }

    let ext_refs: Option<(&std::path::Path, &std::path::Path)> =
        external_cert.as_ref().map(|(c, k)| (c.as_path(), k.as_path()));
    let (cert_chain_der, key_der) = match load_or_generate_cert(ext_refs) {
        Ok(pair) => pair,
        Err(e) => {
            error!("API/TLS: cert provisioning failed: {e}");
            return;
        }
    };

    let cert_chain: Vec<CertificateDer<'static>> = cert_chain_der.into_iter().map(CertificateDer::from).collect();

    // Optional mutual-TLS: require a client cert chained to a PEM
    // bundle of trusted CAs. Used to lock the TLS endpoint behind
    // Cloudflare's Authenticated Origin Pull cert, so the origin
    // only accepts traffic that arrived via CF.
    let client_verifier = match &client_ca {
        Some(path) => match load_client_ca(path) {
            Ok(roots) => match WebPkiClientVerifier::builder(Arc::new(roots)).build() {
                Ok(v) => Some(v),
                Err(e) => {
                    error!("API/TLS: client-CA verifier build failed: {e}");
                    return;
                }
            },
            Err(e) => {
                error!("API/TLS: load client CA {}: {e}", path.display());
                return;
            }
        },
        None => None,
    };
    let builder = ServerConfig::builder();
    let builder = if let Some(v) = client_verifier {
        builder.with_client_cert_verifier(v)
    } else {
        builder.with_no_client_auth()
    };
    let key = match PrivateKeyDer::try_from(key_der) {
        Ok(k) => k,
        Err(e) => {
            error!("API/TLS: private key conversion failed: {e}");
            return;
        }
    };
    let mut cfg = match builder.with_single_cert(cert_chain, key) {
        Ok(c) => c,
        Err(e) => {
            error!("API/TLS: rustls config build failed: {e}");
            return;
        }
    };
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let cfg = Arc::new(cfg);

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("API/TLS: tokio runtime build failed: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match TokioListener::bind(&bind).await {
                Ok(l) => {
                    info!("API: TLS listening on {bind} (HTTP/2 + HTTP/1.1 via ALPN)");
                    l
                }
                Err(e) => {
                    error!("API: failed to bind {bind}: {e}");
                    return;
                }
            };
            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
            let shutdown = Arc::new(tokio::sync::Notify::new());
            tokio::spawn(shutdown_watcher(shutdown.clone()));
            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((stream, _)) = res else { continue };
                        let acceptor = acceptor.clone();
                        let state = state.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            let tls = match acceptor.accept(stream).await {
                                Ok(t) => t,
                                Err(e) => {
                                    debug!("API/TLS: handshake failed: {e}");
                                    return;
                                }
                            };
                            // After ALPN: pick h2 or h1 from the negotiated
                            // protocol so hyper uses the right framing.
                            let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                            let is_h2 = alpn.as_deref() == Some(b"h2");
                            serve_connection(TokioIo::new(tls), is_h2, state, token, read_only).await;
                        });
                    }
                    () = shutdown.notified() => {
                        info!("API/TLS: SIGTERM received, closing :{bind} listener");
                        break;
                    }
                }
            }
        });
    });
}

/// Self-signed cert validity, kept under Chrome's 398-day cap for
/// publicly-trusted certs so cert hygiene matches current browser
/// expectations even though we're not a public CA.
const CERT_VALIDITY_DAYS: i64 = 365;
/// Regenerate when the cert's `not_after` is closer than this many
/// days from now. Gives any device that pinned the previous cert
/// (mobile, browser trust store) a 30-day window to re-pin before
/// the relay starts serving a different cert.
const CERT_RENEW_BEFORE_EXPIRY_DAYS: i64 = 30;

/// Check that we can write `path`. If the file exists, opens it
/// for writing without truncating (so a successful check leaves
/// the file alone). If the file doesn't exist, attempts to create
/// and immediately remove a sibling temp file to probe the parent
/// directory's write permission. Any failure bubbles up so we
/// surface "the cert is on a read-only mount" instead of letting
/// the relay run on a stale cert.
fn ensure_writable(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::OpenOptions::new().write(true).open(path)?;
        return Ok(());
    }
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other(format!(
            "no parent directory for {}",
            path.display()
        )));
    };
    let probe = parent.join(".write-probe");
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Parse the cert's actual `not_after` and decide whether we're
/// within the renewal window. Source of truth is what the cert
/// itself says — not the file's mtime — so importing a cert from
/// another host works correctly. Returns true on any parse error
/// so a malformed cert gets replaced rather than silently kept.
fn cert_needs_renewal(crt_path: &std::path::Path) -> bool {
    let renewal_window = time::Duration::days(CERT_RENEW_BEFORE_EXPIRY_DAYS);
    let Ok(pem_bytes) = std::fs::read(crt_path) else {
        return true;
    };
    // rcgen 0.14 dropped `CertificateParams::from_ca_cert_pem`; use
    // x509-parser directly. Any failure to parse → renew (the file
    // is broken, regen will replace it).
    let Ok((_, pem)) = x509_parser::pem::parse_x509_pem(&pem_bytes) else {
        return true;
    };
    let Ok(cert) = pem.parse_x509() else {
        return true;
    };
    let Ok(not_after) = time::OffsetDateTime::from_unix_timestamp(cert.validity().not_after.timestamp()) else {
        return true;
    };
    let now = time::OffsetDateTime::now_utc();
    not_after - now < renewal_window
}

/// Parse a PEM bundle of CA certificates into a `RootCertStore` for
/// client-cert verification (mTLS / Cloudflare Authenticated Origin
/// Pulls). Each `-----BEGIN CERTIFICATE-----` block in the file is
/// added as a trust anchor.
fn load_client_ca(path: &std::path::Path) -> std::io::Result<rustls::RootCertStore> {
    let bytes = std::fs::read(path)?;
    let mut roots = rustls::RootCertStore::empty();
    let mut added = 0usize;
    for der in rustls_pemfile::certs(&mut bytes.as_slice()).filter_map(Result::ok) {
        if roots.add(der).is_ok() {
            added += 1;
        }
    }
    if added == 0 {
        return Err(std::io::Error::other(format!(
            "no CA cert added from {} (file empty or all certs rejected)",
            path.display()
        )));
    }
    info!("API/TLS: loaded {added} client-CA root(s) from {}", path.display());
    Ok(roots)
}

/// Load a user-supplied PEM cert + key pair (e.g. a Cloudflare
/// Origin certificate). Multi-cert PEM files are loaded as a chain
/// (leaf first, then intermediate(s)) so clients without the issuing
/// CA in their trust store can still build a path. Renewal is the
/// operator's responsibility — we never modify these files.
fn load_external_cert(
    crt_path: &std::path::Path,
    key_path: &std::path::Path,
) -> std::io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let crt_pem = std::fs::read(crt_path)
        .map_err(|e| std::io::Error::other(format!("read TLS cert {}: {e}", crt_path.display())))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| std::io::Error::other(format!("read TLS key {}: {e}", key_path.display())))?;
    let chain: Vec<Vec<u8>> = rustls_pemfile::certs(&mut crt_pem.as_slice())
        .filter_map(Result::ok)
        .map(|c| c.to_vec())
        .collect();
    if chain.is_empty() {
        return Err(std::io::Error::other(format!(
            "no PEM CERTIFICATE block in {}",
            crt_path.display()
        )));
    }
    let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| std::io::Error::other(format!("parse TLS key {}: {e}", key_path.display())))?
        .ok_or_else(|| std::io::Error::other(format!("no PEM PRIVATE KEY block in {}", key_path.display())))?
        .secret_der()
        .to_vec();
    Ok((chain, key_der))
}

/// Returns the chain (leaf first) + key. Falls back to a self-signed
/// cert in the state dir when `external` is `None`.
fn load_or_generate_cert(
    external: Option<(&std::path::Path, &std::path::Path)>,
) -> std::io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
    if let Some((crt, key)) = external {
        info!(
            "API/TLS: loading user-supplied cert {} + key {}",
            crt.display(),
            key.display()
        );
        return load_external_cert(crt, key);
    }
    let dir = crate::platform::state_base_dir().join(crate::APP_DIR);
    std::fs::create_dir_all(&dir)?;
    let crt_path = dir.join("tls.crt");
    let key_path = dir.join("tls.key");

    if crt_path.exists() && key_path.exists() && !cert_needs_renewal(&crt_path) {
        let crt_pem = std::fs::read(&crt_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let cert_der = rustls_pemfile::certs(&mut crt_pem.as_slice())
            .next()
            .and_then(Result::ok)
            .ok_or_else(|| std::io::Error::other("no cert in tls.crt"))?
            .to_vec();
        let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())?
            .ok_or_else(|| std::io::Error::other("no key in tls.key"))?
            .secret_der()
            .to_vec();
        return Ok((vec![cert_der], key_der));
    }
    if crt_path.exists() {
        info!(
            "API/TLS: cert within {CERT_RENEW_BEFORE_EXPIRY_DAYS} days of expiry (or unparseable), regenerating at {}",
            dir.display()
        );
    } else {
        info!("API/TLS: generating self-signed certificate at {}", dir.display());
    }

    // Bail loudly if we can't actually write the target files. A
    // half-finished regeneration would leave the relay either using
    // a stale cert (silently) or no cert at all (silently). Better
    // to fail fast so the user sees the permission problem and
    // decides what to do with the existing files.
    ensure_writable(&crt_path)?;
    ensure_writable(&key_path)?;

    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string(), local_ip()])
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "tab-atelier");
    // rcgen's defaults are `not_before = 1975-01-01` and
    // `not_after = 4096-01-01`. That's syntactically valid but
    // unusual — pin the window to (now, now + 365d), under Chrome's
    // 398-day cap. Renewal is handled at the call site above by
    // checking file mtime on each startup.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(CERT_VALIDITY_DAYS);
    let key_pair = rcgen::KeyPair::generate().map_err(|e| std::io::Error::other(e.to_string()))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let crt_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    std::fs::write(&crt_path, &crt_pem)?;
    // The TLS private key must never be world-readable — a local user
    // who reads it can impersonate / MITM the API's TLS listener. Match
    // the 0600 handling used for api.token. Create with O_EXCL + mode so
    // the key never exists on disk with looser perms, even briefly;
    // fall back to write+chmod if the file already exists.
    write_private_file(&key_path, key_pem.as_bytes())?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    Ok((vec![cert_der], key_der))
}
