// SPDX-License-Identifier: MPL-2.0

//! The HTTP server: bind, accept, dispatch to a local asset or the daemon.

use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response, StatusCode, header};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::body::{BoxBody, full};
use crate::config::{Config, HopTimeouts, Upstream};
use crate::proxy::proxy;
use crate::route::{self, Route};

/// Bind `cfg.bind` and serve until the process is killed.
///
/// # Errors
///
/// Returns an error if the address cannot be bound.
pub async fn run(cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    serve(listener, cfg.upstream).await
}

/// Serve on an already-bound listener.
///
/// Split from [`run`] so a test can bind port 0 and learn the port it got
/// rather than guessing a free one and racing the machine.
///
/// # Errors
///
/// Does not return errors: a failed `accept` is retried and a failed connection
/// is dropped. The `Result` matches [`run`]'s shape for callers.
pub async fn serve(listener: TcpListener, upstream: String) -> std::io::Result<()> {
    serve_with(listener, upstream, HopTimeouts::default()).await
}

/// [`serve`] with explicit hop deadlines, so a test can compress them and still
/// exercise the real timeout paths instead of waiting out the production values.
pub async fn serve_with(listener: TcpListener, upstream: String, timeouts: HopTimeouts) -> std::io::Result<()> {
    let upstream = Arc::new(Upstream::new(upstream, timeouts));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A single failed accept (fd exhaustion, client gone) must not take
            // the kiosk down with it; the next connection is unaffected.
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

/// Serve a local asset, or relay the request to the daemon.
async fn handle(req: Request<Incoming>, upstream: &Upstream) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();
    match route::route(&Request::from_parts(parts.clone(), ())) {
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
