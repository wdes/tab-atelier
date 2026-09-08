// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTTP surface: the proxied Anthropic path, the admin API behind it, and
//! the web UI.
//!
//! Two separate credentials, on purpose:
//!
//! * a **user key** ([`crate::users`]) opens `/relay/anthropic/*` and nothing
//!   else. It lives in a developer's `ANTHROPIC_BASE_URL` environment, so it
//!   travels widely and must not be able to administer anything.
//! * the **admin token** opens `/api/*` and the UI. It stays on the server.
//!
//! A user key rejected by `/api/users` is not a mistake to smooth over; it is
//! the boundary working.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response};

use crate::egress;
use crate::users::{Account, Store, constant_time_eq};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// Everything a request handler needs.
pub struct State {
    pub store: Mutex<Store>,
    pub admin_token: String,
    pub web_root: Option<std::path::PathBuf>,
}

fn text(code: u16, msg: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

fn json(code: u16, body: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        // The admin API is same-origin only: no CORS headers, so a page on
        // another origin cannot read a response even if it can send a request.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

fn account_json(a: &Account) -> serde_json::Value {
    // Never the key hash. It is not a secret you can use, but it is the input
    // to an offline guess, and the UI has no reason to hold it.
    serde_json::json!({
        "id": a.id,
        "first_name": a.first_name,
        "last_name": a.last_name,
        "email": a.email,
        "created_at": a.created_at,
        "last_used_at": a.last_used_at,
        "disabled": a.disabled,
        "has_key": !a.key_hash.is_empty(),
    })
}

/// Pull a bearer-ish credential out of either header a client might use.
///
/// A claude client sends `x-api-key`; our own forwarding hop and the web UI
/// send `Authorization: Bearer`. Accepting both from the start avoids the
/// class of bug where the credential is right and the envelope is not — which
/// produces a 401 that tells the operator nothing.
fn presented(req: &Request<Incoming>) -> String {
    req.headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            req.headers()
                .get(hyper::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Serve one request.
///
/// # Errors
/// Never — the `Result` is `Infallible`, and exists only because hyper's
/// `service_fn` requires one. Failures become status codes, not errors: a
/// proxy that drops a connection instead of answering 502 tells the client
/// nothing about what went wrong.
pub async fn handle(req: Request<Incoming>, state: Arc<State>) -> Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    if path.starts_with("/relay/anthropic") {
        return Ok(anthropic(req, state).await);
    }
    if path.starts_with("/api/") {
        return Ok(admin(req, state).await);
    }
    if method == Method::GET || method == Method::HEAD {
        return Ok(web(&path, &state));
    }
    Ok(text(404, "not found"))
}

// ── the proxied path ────────────────────────────────────────────────

async fn anthropic(req: Request<Incoming>, state: Arc<State>) -> Response<Body> {
    let method = req.method().clone();
    let sub = req
        .uri()
        .path()
        .strip_prefix("/relay/anthropic")
        .unwrap_or("")
        .to_owned();
    let sub_pq = req.uri().query().map_or_else(|| sub.clone(), |q| format!("{sub}?{q}"));

    // Claude Code probes reachability with an UNAUTHENTICATED HEAD/GET before
    // it has a credential to present. Answering 401 here reads as "this
    // endpoint is broken" and the client never attempts a real call, so this
    // is answered first — and kept to an exact path, because an
    // unauthenticated branch is security surface.
    if sub == "/api/hello" && matches!(method, Method::HEAD | Method::GET) {
        let body = if method == Method::HEAD { "" } else { "{}" };
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)).boxed())
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()));
    }

    let key = presented(&req);
    let who = {
        let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = store.authenticate(&key).cloned();
        if let Some(ref a) = found {
            store.touch(&a.id);
        }
        found
    };
    let Some(account) = who else {
        // Say which of the two credentials was wrong without printing either.
        // "unauthorized" alone leaves an operator guessing between a revoked
        // account, a typo, and the admin token in the wrong place.
        let diagnosis = if key.is_empty() {
            "no key presented"
        } else if constant_time_eq(key.as_bytes(), state.admin_token.as_bytes()) {
            "that is the ADMIN token — the proxy path takes a user key"
        } else {
            "no active account has that key (revoked, disabled, or mistyped)"
        };
        log::warn!(
            "proxy: 401 on {method} /relay/anthropic{sub} ({} chars presented): {diagnosis}",
            key.chars().count()
        );
        return text(401, &format!("tab-atelier-proxy: unauthorized ({diagnosis})"));
    };
    log::info!(
        "proxy: {method} {sub} for {} <{}>",
        account.display_name(),
        account.email
    );

    let client_beta = req
        .headers()
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let is_post = method == Method::POST;
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(400, "bad request body"),
    };

    let fwd = Forward {
        sub_pq,
        is_post,
        content_type,
        client_beta,
        body,
    };
    // Bridge ureq's blocking reader to an async hyper stream: the blocking task
    // sends (status, content-type) over a oneshot, then pumps chunks over an
    // mpsc. Without this an SSE response would only reach the client once the
    // whole generation finished, which for a long answer looks like a hang.
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<Result<(u16, Option<String>), String>>();
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::task::spawn_blocking(move || forward(fwd, meta_tx, &body_tx));

    let meta = match meta_rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return text(502, &format!("tab-atelier-proxy: {e}")),
        Err(_) => return text(502, "tab-atelier-proxy: forward task died"),
    };
    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, Infallible>(Frame::data(b)), rx))
    });
    let mut builder = Response::builder().status(meta.0);
    if let Some(ct) = meta.1 {
        builder = builder.header("content-type", ct);
    }
    builder
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// One request on its way upstream, moved into the blocking task.
struct Forward {
    sub_pq: String,
    is_post: bool,
    content_type: String,
    client_beta: Option<String>,
    body: Bytes,
}

/// The blocking half: authenticate to Anthropic, send, and pump the response.
///
/// The upstream status and body are passed through untouched, including error
/// statuses. A proxy that turned a 429 into its own 502 would hide both the
/// reason and the `retry-after` the client needs.
fn forward(
    f: Forward,
    // By value: a oneshot Sender is consumed by `send`, which is also what
    // makes "exactly one answer" a type-level guarantee rather than a habit.
    meta_tx: tokio::sync::oneshot::Sender<Result<(u16, Option<String>), String>>,
    body_tx: &tokio::sync::mpsc::Sender<Bytes>,
) {
    // Deliberately not held anywhere: fetched per request so a refreshed token
    // is picked up without restarting, and never written to a log.
    let token = match egress::oauth_access_token() {
        Ok(t) => t,
        Err(e) => {
            let _ = meta_tx.send(Err(format!("egress oauth: {e}")));
            return;
        }
    };
    let url = format!("{}{}", egress::upstream(), f.sub_pq);
    let agent = egress::relay_agent();
    let hdrs: Vec<(&str, String)> = vec![
        ("Content-Type", f.content_type),
        ("Authorization", format!("Bearer {token}")),
        ("anthropic-version", egress::ANTHROPIC_VERSION.to_owned()),
        // The client's own beta flags are merged in, not replaced: a body field
        // gated behind a flag the client opted into is rejected upstream as an
        // unknown input if only our flags survive.
        (
            "anthropic-beta",
            egress::merge_beta(f.client_beta.as_deref(), egress::ANTHROPIC_BETA),
        ),
    ];
    let sent = if f.is_post {
        let mut rb = agent.post(&url);
        for (k, v) in &hdrs {
            rb = rb.header(*k, v);
        }
        rb.send(&f.body[..])
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
            Ok(0) | Err(_) => break, // EOF, or an upstream read error
            Ok(n) => {
                if body_tx.blocking_send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                    break; // client hung up
                }
            }
        }
    }
}

// ── the admin API ───────────────────────────────────────────────────

async fn admin(req: Request<Incoming>, state: Arc<State>) -> Response<Body> {
    if state.admin_token.is_empty() {
        return json(503, r#"{"error":"no admin token configured"}"#);
    }
    if !constant_time_eq(presented(&req).as_bytes(), state.admin_token.as_bytes()) {
        return json(401, r#"{"error":"admin token required"}"#);
    }
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return json(400, r#"{"error":"bad body"}"#),
    };
    let field = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
    let parsed = |b: &Bytes| serde_json::from_slice::<serde_json::Value>(b).unwrap_or(serde_json::Value::Null);
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    match (method, path.as_str()) {
        (Method::GET, "/api/users") => {
            let list: Vec<_> = store.accounts().iter().map(account_json).collect();
            json(200, &serde_json::json!({ "users": list }).to_string())
        }
        (Method::POST, "/api/users") => {
            let v = parsed(&body);
            match store.add(&field(&v, "first_name"), &field(&v, "last_name"), &field(&v, "email")) {
                // The key is in this response and in no other, ever. The UI
                // shows it once and says so.
                Ok((a, key)) => json(
                    201,
                    &serde_json::json!({ "user": account_json(&a), "key": key }).to_string(),
                ),
                Err(e) => json(400, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (Method::POST, p) if p.ends_with("/rotate") => {
            let who = p.trim_start_matches("/api/users/").trim_end_matches("/rotate");
            match store.rotate(who) {
                Ok((a, key)) => json(
                    200,
                    &serde_json::json!({ "user": account_json(&a), "key": key }).to_string(),
                ),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (Method::POST, p) if p.ends_with("/disabled") => {
            let who = p.trim_start_matches("/api/users/").trim_end_matches("/disabled");
            let disabled = parsed(&body)
                .get("disabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            match store.set_disabled(who, disabled) {
                Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (Method::DELETE, p) if p.starts_with("/api/users/") => {
            match store.remove(p.trim_start_matches("/api/users/")) {
                Ok(a) => json(200, &serde_json::json!({ "removed": account_json(&a) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        _ => json(404, r#"{"error":"no such endpoint"}"#),
    }
}

// ── the web UI ──────────────────────────────────────────────────────

/// Serve the UI from `web_root`.
///
/// Path traversal is refused by rejecting any `..` component outright rather
/// than canonicalising and comparing: there is nothing under this root a
/// caller should reach by climbing, so the simplest rule that cannot be
/// subtly wrong is the right one.
fn web(path: &str, state: &State) -> Response<Body> {
    let Some(root) = state.web_root.as_ref() else {
        return text(404, "web UI not installed");
    };
    let rel = match path.trim_start_matches('/') {
        "" => "index.html",
        other => other,
    };
    if rel.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        return text(400, "bad path");
    }
    let full = root.join(rel);
    let Ok(bytes) = std::fs::read(&full) else {
        return text(404, "not found");
    };
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map" | "json") => "application/json",
        _ => "application/octet-stream",
    };
    Response::builder()
        .status(200)
        .header("content-type", ctype)
        // The UI holds an admin token in memory; keep it out of any embedding
        // page and out of a referrer.
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .body(Full::new(Bytes::from(bytes)).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Run until Ctrl-C.
///
/// # Errors
/// The address could not be bound.
pub async fn serve(addr: SocketAddr, state: Arc<State>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!("tab-atelier-proxy listening on http://{addr}");
    serve_on(listener, state).await
}

/// Serve on a listener that is already bound.
///
/// Split out so a test can bind port 0, learn which port it got, and then hand
/// the listener over — the alternative is guessing a free port, which is flaky
/// on a busy machine.
///
/// # Errors
/// Does not return in normal operation.
pub async fn serve_on(listener: tokio::net::TcpListener, state: Arc<State>) -> Result<(), String> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, Arc::clone(&state)));
            let _ = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .timer(hyper_util::rt::TokioTimer::new())
                // Slow-loris guard: bound how long a client may take to dribble
                // in its headers, or one byte every few seconds ties up a task
                // forever and the accept loop spawns one per connection.
                .header_read_timeout(std::time::Duration::from_secs(30))
                .serve_connection(io, svc)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_web_root_cannot_be_climbed_out_of() {
        let state = State {
            store: Mutex::new(Store::load(std::env::temp_dir().join("ta-proxy-web-test.json")).expect("store")),
            admin_token: "t".to_owned(),
            web_root: Some(std::path::PathBuf::from("/usr/share/tab-atelier-proxy/web")),
        };
        for attack in [
            "/../../../../etc/passwd",
            "/vendor/../../../etc/shadow",
            "/./secret",
            "//etc/passwd",
        ] {
            let resp = web(attack, &state);
            assert!(
                resp.status() == 400 || resp.status() == 404,
                "{attack} was not refused: {}",
                resp.status()
            );
        }
    }

    #[test]
    fn an_account_serialises_without_its_hash() {
        let a = Account {
            id: "id-1".to_owned(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            email: "ada@example.org".to_owned(),
            key_hash: "deadbeef".to_owned(),
            created_at: 1,
            last_used_at: None,
            disabled: false,
        };
        let json = account_json(&a).to_string();
        assert!(!json.contains("deadbeef"), "the key hash must not reach the UI: {json}");
        assert!(json.contains("ada@example.org"));
        assert!(json.contains("\"has_key\":true"));
    }
}
