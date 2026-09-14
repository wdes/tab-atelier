// SPDX-License-Identifier: MPL-2.0

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
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};

use crate::users::Store;
use crate::{account, inspect, provider, qos, usage};

use crate::transport::{InReq, Reply};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// Everything a request handler needs.
pub struct State {
    pub store: Mutex<Store>,
    /// Separate lock from the accounts: recording usage happens on every
    /// proxied request, and it must not queue behind an admin listing users.
    pub usage: Mutex<usage::Store>,
    /// Who goes next when the shared quota is tight.
    pub sched: Mutex<qos::Sched>,
    /// How much of the shared plan has been USED, polled from upstream.
    /// Consumed, not remaining: 1.0 means exhausted.
    pub account: Mutex<account::Monitor>,
    /// Everywhere a request can go.
    ///
    /// Behind a lock because it is editable at runtime: an operator adds a
    /// provider from the UI and the next request uses it, with no restart.
    /// Held only for the read, never across a request to an upstream.
    pub registry: Mutex<provider::Registry>,
    /// Where the registry is written back to when it changes.
    pub registry_path: std::path::PathBuf,
    /// Providers upstream has told us to leave alone, and until when (unix
    /// seconds). Keyed by provider id, because a 429 from one says nothing
    /// about another — that is the entire point of having more than one.
    pub provider_backoff: Mutex<std::collections::BTreeMap<String, u64>>,
    /// Woken when capacity frees up, so a queued call retries promptly instead
    /// of sitting out its full backoff.
    pub wake: tokio::sync::Notify,
    /// Captured requests, when inspection is armed. Off by default and
    /// self-disarming — see [`crate::inspect`].
    pub inspect: Mutex<inspect::Store>,
    pub admin_token: String,
    pub web_root: Option<std::path::PathBuf>,
}

impl State {
    /// A state with nothing configured, for tests that exercise a guard rather
    /// than the machinery behind it.
    ///
    /// Every store starts empty and no provider is registered, which is what
    /// makes this useful: a test that passes here is testing its own rule and
    /// not a fixture. The three stores that only load from disk are pointed at
    /// a path under `target/` that does not exist, which is how the server
    /// itself starts on a fresh installation.
    ///
    /// # Panics
    ///
    /// If the user store cannot be read. Under `target/`, beside the manifest,
    /// a missing file is a fresh store, so a panic here means a broken checkout
    /// rather than anything the test did.
    #[cfg(test)]
    #[must_use]
    pub fn for_tests(admin_token: String) -> Self {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/for-tests");
        Self {
            store: Mutex::new(Store::load(dir.join("users.json")).expect("fresh store")),
            usage: Mutex::new(usage::Store::load(&dir)),
            sched: Mutex::new(qos::Sched::new()),
            account: Mutex::new(account::Monitor::load(&dir)),
            registry: Mutex::new(provider::Registry::default()),
            registry_path: dir.join("providers.json"),
            provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
            wake: tokio::sync::Notify::new(),
            inspect: Mutex::new(inspect::Store::load(&dir)),
            admin_token,
            web_root: None,
        }
    }
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Serve one request.
///
/// # Errors
/// Never — the `Result` is `Infallible`, and exists only because hyper's
/// `service_fn` requires one. Failures become status codes, not errors: a
/// proxy that drops a connection instead of answering 502 tells the client
/// nothing about what went wrong.
pub async fn handle(
    req: Request<Incoming>,
    state: Arc<State>,
    peer: std::net::IpAddr,
) -> Result<Response<Body>, Infallible> {
    // The body is read once, here, so no handler has to think about it and the
    // transport-neutral dispatcher can take a complete request. A body that
    // cannot be read at all is answered as an empty one rather than dropped, so
    // a malformed request still gets a reply instead of a closed connection.
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let query = parts.uri.query().unwrap_or_default().to_owned();
    let req = InReq {
        method: parts.method,
        path: parts.uri.path().to_owned(),
        query,
        headers: parts.headers,
        body,
        peer,
    };

    // Collecting the reply before converting keeps the dispatcher free of
    // transport types. Only the final conversion names hyper.
    Ok(route(&req, &state).await.into_hyper())
}

/// Listen on `addr` and serve until the process ends.
///
/// # Errors
/// If the address cannot be bound — typically a port already in use, or one
/// below 1024 without the privilege for it.
pub async fn serve(addr: SocketAddr, state: Arc<State>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!("tab-atelier-proxy listening on http://{addr}");
    serve_on(listener, state).await
}

/// The same loop against a listener the caller already binds.
///
/// Taking a bound listener is what lets a test pick port 0, learn the port the
/// kernel chose, and drive the real socket instead of a mock.
///
/// # Errors
/// Never in the current loop: `accept` failures are logged and retried, since a
/// per-connection failure (a reset, a file-descriptor spike) must not take the
/// listener down with it.
pub async fn serve_on(listener: tokio::net::TcpListener, state: Arc<State>) -> Result<(), String> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, Arc::clone(&state), peer.ip()));
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

/// Send a request to the route table, which owns every path.
///
/// This module keeps the socket, the state and the shutdown; what answers a
/// path lives in [`crate::http::routes`]. The seam is the transport types, not
/// hyper, which is what lets the whole API be tested without a socket.
async fn route(req: &InReq, state: &Arc<State>) -> Reply {
    crate::http::routes::dispatch(req, state).await
}

#[cfg(test)]
mod tests {
    //! The socket-level tests live beside the handlers they drive; this
    //! module keeps only what is still in this file.
}
