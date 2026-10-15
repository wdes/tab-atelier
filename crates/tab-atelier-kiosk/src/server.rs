// SPDX-License-Identifier: MPL-2.0

//! The HTTP server: bind, accept, dispatch to an asset, a pane, or the daemon.

use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::body::{BoxBody, full};
use crate::config::{Config, HopTimeouts, Upstream};
use crate::daemon::Daemon;
use crate::panes::Panes;
use crate::proxy::{MAX_REQUEST_BODY, proxy};
use crate::route::{self, Route};

/// Bind `cfg.bind` and serve until the process is killed.
///
/// # Errors
///
/// Returns an error if the address cannot be bound.
pub async fn run(cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    serve(listener, cfg).await
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
pub async fn serve(listener: TcpListener, cfg: Config) -> std::io::Result<()> {
    serve_with(listener, cfg, HopTimeouts::default()).await
}

/// [`serve`] with explicit hop deadlines, so a test can compress them and still
/// exercise the real timeout paths instead of waiting out the production values.
pub async fn serve_with(listener: TcpListener, cfg: Config, timeouts: HopTimeouts) -> std::io::Result<()> {
    let upstream = Arc::new(Upstream::new(cfg.upstream, timeouts));
    let daemon = Arc::new(Daemon::new(timeouts));
    let panes = Arc::new(Panes::new(cfg.journal, cfg.token, cfg.remotes, cfg.intentions));
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
        let panes = Arc::clone(&panes);
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let upstream = Arc::clone(&upstream);
                let panes = Arc::clone(&panes);
                let daemon = Arc::clone(&daemon);
                async move { Ok::<_, std::convert::Infallible>(handle(req, &upstream, &panes, &daemon).await) }
            });
            // Connection-level errors (client hung up mid-response) are routine.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Serve a local asset, answer a pane, or relay the request to the daemon.
async fn handle(req: Request<Incoming>, upstream: &Upstream, panes: &Panes, daemon: &Daemon) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();
    let authorization = parts.headers.get(header::AUTHORIZATION).cloned();
    let route = route::route(&Request::from_parts(parts.clone(), ()));

    // One gate for every pane and the remote API, before the match, rather than
    // the same check repeated in each arm. A method error is answered before it:
    // the verb is wrong whether or not the caller holds the token.
    if route.is_protected() && !panes.authorized(authorization.as_ref()) {
        return Panes::unauthorized();
    }

    match route {
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
        Route::Decisions { include_archived } => panes.list(include_archived),
        Route::DecisionFile { path } => panes.file(&path),
        Route::DecisionRead { id } => panes.read(&id),
        Route::DecisionTranch { id } => match read_field(body, "verdict").await {
            Ok(verdict) => panes.tranch(&id, &verdict),
            Err(response) => response,
        },
        Route::Reports => panes.reports(),
        Route::Intent => match read_field(body, "content").await {
            Ok(content) => panes.intent(&content),
            Err(response) => response,
        },
        Route::IntentList => panes.intents_list(),
        Route::IntentNew => match read_json(body).await {
            Ok(value) => {
                let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or("");
                panes.intent_new(field("title"), field("repo"), field("pitch"))
            }
            Err(response) => response,
        },
        Route::IntentView { slug } => panes.intent_view(&slug),
        Route::IntentPromote { slug } => panes.intent_promote(&slug),
        Route::IntentAsk { slug } => match read_field(body, "text").await {
            Ok(text) => panes.intent_ask(&slug, &text),
            Err(response) => response,
        },
        Route::Catalog => Panes::catalog(),
        Route::RemotesList => panes.remotes_list(),
        Route::RemoteAdd => match read_json(body).await {
            Ok(value) => {
                let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or("");
                panes.remote_add(field("label"), field("url"), field("token"))
            }
            Err(response) => response,
        },
        Route::RemoteRemove { id } => panes.remote_remove(&id),
        Route::RemoteTabs { id } => remote_call(panes, daemon, &id, "/tabs", None).await,
        Route::RemoteTabCreate { id } => match read_json(body).await {
            Ok(spec) => remote_call(panes, daemon, &id, "/tabs", Some(spec)).await,
            Err(response) => response,
        },
        Route::MethodNotAllowed(allow) => Panes::method_not_allowed(allow),
    }
}

/// Reaches a configured remote with its token, server-side, and relays the
/// answer. The browser sent only the Kiosk token; the remote's never leaves here.
async fn remote_call(
    panes: &Panes,
    daemon: &Daemon,
    id: &str,
    path: &str,
    spec: Option<serde_json::Value>,
) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    // ureq is blocking, so the hop runs on a blocking thread and the reactor
    // keeps serving other connections while a slow remote answers.
    let daemon = daemon.clone();
    let path = path.to_string();
    let answered = tokio::task::spawn_blocking(move || {
        spec.map_or_else(
            || daemon.get(&remote, &path),
            |value| daemon.post(&remote, &path, &value),
        )
    })
    .await;
    match answered {
        Ok(Ok((status, text))) => relay_response(status, text),
        // The reason may name an internal host: the caller is told only that the
        // remote is the one that failed.
        _ => Panes::bad_gateway(),
    }
}

/// Passes a remote's own answer through: status, and the JSON body as it came.
fn relay_response(status: u16, text: String) -> Response<BoxBody> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(text))
        .expect("a status and body from a real response")
}

/// Reads a JSON request body, bounded like any relayed body.
async fn read_json(body: Incoming) -> Result<serde_json::Value, Response<BoxBody>> {
    let bytes = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(Panes::bad_request()),
    };
    serde_json::from_slice(&bytes).map_err(|_| Panes::bad_request())
}

/// Reads one string field out of a JSON request body, bounded like any relayed
/// body. A missing field and malformed JSON are the same 400: the pane knows
/// what it asked for and got neither.
async fn read_field(body: Incoming, field: &str) -> Result<String, Response<BoxBody>> {
    let bytes = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(Panes::bad_request()),
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get(field).and_then(|v| v.as_str()).map(str::to_string))
        .ok_or_else(Panes::bad_request)
}
