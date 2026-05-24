// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! happier-relay — single-tenant Rust server speaking the happier
//! (`happier-dev/happier`) wire protocol. Phase-1 spike: `/v1/auth`
//! and `/v1/auth/ping` only.

#![allow(clippy::module_name_repetitions)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete as axum_delete, get, post},
    Json, Router,
};
use clap::Parser;
use socketioxide::SocketIo;
use tracing_subscriber::EnvFilter;

mod artifacts;
mod auth;
mod changes;
mod db;
mod features;
mod jwt;
mod kv;
mod pairing;
mod sessions;
mod socket;
mod sse;
mod state;
mod stubs;
mod tab_input;
mod web;

#[derive(Parser, Debug)]
#[command(version, about = "Single-tenant happier relay (auth spike).", long_about = None)]
struct Args {
    /// TCP port to listen on. Pick something the happier CLI's
    /// `--server-url http://localhost:PORT` flag can reach.
    #[arg(long, default_value_t = 8082)]
    port: u16,

    /// Bind address. Default is loopback only — passing `0.0.0.0`
    /// exposes the relay on the LAN (mobile client on the same wifi).
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// `SQLite` database path.
    ///
    /// Defaults to `~/.local/state/happier-relay/db.sqlite`.
    #[arg(long)]
    db_path: Option<PathBuf>,

    /// Master secret for JWT signing. Either the literal value or
    /// `env:VAR_NAME` to read from the environment. Required.
    #[arg(long)]
    master_secret: String,

    /// Pin a single owner's Ed25519 public key (hex). When absent, the
    /// first successful `/v1/auth` call defines the owner implicitly.
    #[arg(long)]
    owner_pubkey: Option<String>,

    /// When set, every successful `/v1/auth` binds to the *same*
    /// account regardless of which keypair signed the challenge.
    ///
    /// The first auth seeds the shared account; subsequent auths reuse
    /// it. Designed for a single-user self-hosted setup where
    /// tab-atelier + web UI + mobile all need to see one another's
    /// artifacts. Trust model: anyone who can reach the relay can join
    /// the account, so run on loopback or a private network.
    #[arg(long)]
    shared_account: bool,

    /// PEM-encoded TLS certificate file. When passed together with
    /// `--tls-key`, the relay listens over HTTPS instead of plain
    /// HTTP. Required for the happier mobile app, which blocks
    /// cleartext traffic on modern Android / iOS.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// PEM-encoded TLS private key file. Pairs with `--tls-cert`.
    #[arg(long)]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // route registration dominates
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,happier_relay=debug")))
        .init();

    let args = Args::parse();
    let secret = resolve_secret(&args.master_secret)?;
    let db_path = args.db_path.unwrap_or_else(default_db_path);
    let pool = db::open(&db_path).await?;

    // socket.io v4 and the fan-out task. The HTTP handlers don't talk
    // to SocketIo directly (its broadcast future isn't Send across
    // axum boundaries); they push into an mpsc, and a dedicated task
    // owns the handle and drains the channel.
    //
    // happier's mobile UI dials path `/v1/updates/` (see
    // `apps/ui/sources/sync/api/session/connection/
    // createSyncSocketTransport.ts`). The default `/socket.io/` makes
    // every reconnect attempt land on a 404 → the UI's status panel
    // shows "Socket: disconnected" with "server error". Match the
    // upstream wire path.
    let (socket_layer, io) = SocketIo::builder()
        .req_path("/v1/updates/")
        .build_layer();
    let (broadcast_tx, _broadcast_rx_initial) = tokio::sync::broadcast::channel::<state::BroadcastMsg>(256);

    let machine_id = resolve_machine_id();
    let state = state::AppState {
        db: pool,
        jwt_secret: Arc::new(secret),
        owner_pubkey_hex: args.owner_pubkey.map(|s| s.to_lowercase()),
        shared_account: args.shared_account,
        machine_id,
        broadcast_tx: broadcast_tx.clone(),
        input_notifier: tab_input::InputNotifier::default(),
        changes: changes::ChangesRingBuffer::default(),
    };

    tokio::spawn(state::broadcast_loop(io.clone(), broadcast_tx.subscribe()));

    let connect_state = state.clone();
    io.ns("/", move |socket: socketioxide::extract::SocketRef, data: socketioxide::extract::Data<socket::AuthPayload>| {
        let st = connect_state.clone();
        async move { socket::on_connect_with_state(socket, data, st).await }
    });

    // Authed routes get the middleware; public ones (just /v1/auth) don't.
    let authed = Router::new()
        .route("/v1/auth/ping", get(auth::ping_handler))
        .route("/v1/sessions", post(sessions::create).get(sessions::list_all))
        .route("/v1/sessions/{id}", axum_delete(sessions::delete))
        .route("/v2/sessions/{id}", get(dispatch_session_get_v2).patch(sessions::patch))
        .route("/v1/sessions/{id}/messages", get(dispatch_session_messages_v1))
        .route("/v2/sessions/{id}/messages", post(sessions::post_message))
        .route("/v1/kv", get(kv::list).post(kv::mutate))
        .route("/v1/kv/bulk", post(kv::bulk_get))
        .route("/v1/kv/{key}", get(kv::get_one))
        .route("/v1/artifacts", post(artifacts::create).get(artifacts::list))
        .route(
            "/v1/artifacts/{id}",
            get(artifacts::get_one).post(artifacts::update).delete(artifacts::delete),
        )
        .route("/v1/artifacts/{id}/append", post(artifacts::append))
        .route("/v1/tab-input", post(tab_input::post_input))
        .route("/v1/tab-input/pending", get(tab_input::pending))
        .route("/v1/events", get(sse::events))
        // Mobile UI stubs — minimal-shape returns so the homepage,
        // machine list, account screens, and sync poll all stop
        // 404'ing. Real data wiring is a follow-up; the goal here
        // is "no spinner forever" / "no 'offline' badge".
        .route("/v1/machines", get(stubs::list_machines))
        .route("/v2/sessions", get(stubs::list_sessions_v2))
        .route("/v1/account/profile", get(stubs::account_profile))
        .route("/v1/account/encryption", get(stubs::account_encryption))
        .route("/v2/changes", get(changes::changes))
        .route("/v2/cursor", get(changes::cursor))
        .route("/v1/push-tokens", get(stubs::list_push_tokens).post(stubs::register_push_token))
        .route("/v1/push-tokens/{token}", axum_delete(stubs::delete_push_token))
        .route("/v1/feed", get(stubs::feed))
        .route("/v1/friends", get(stubs::friends))
        .route("/v1/account/activity/badge-snapshot", get(stubs::activity_badge_snapshot))
        // Session-detail aux endpoints the mobile fires when opening
        // a chat view. Empty payloads keep the page from spinning.
        .route("/v2/sessions/{id}/pending", get(stubs::tab_session_pending))
        .route("/v2/session-folder-assignments", get(stubs::session_folder_assignments))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        // Some clients (and casual `curl http://host:7892`) probe the
        // root to confirm the server is up; return a minimal JSON
        // discovery doc instead of a 404 so liveness checks pass.
        .route("/", get(root_discovery))
        // Liveness probe used by the mobile app's sync layer before
        // it'll attempt anything else. Must return `{ok: true}` with
        // no auth, matching `apps/server/sources/app/api/routes/
        // version/versionRoutes.ts` in happier upstream.
        .route("/v1/version", get(version_probe))
        // Connectivity gate the mobile homepage uses. Plain text
        // "ok", status 200, no auth — see
        // `apps/ui/sources/sync/http/client.connectivityGate.test.ts`.
        .route("/health", get(health_probe))
        .route("/v1/auth", post(auth::auth_handler))
        // Mobile clients (happier UI) probe `GET /v1/features` over
        // 800 ms before doing anything else; if it 404s they decide
        // the server is incompatible and abort.
        .route("/v1/features", get(features::features))
        // Pairing-style auth used by the mobile app. The desktop CLI
        // uses `/v1/auth` (Ed25519 challenge); the mobile UI uses this
        // flow. In single-tenant `--shared-account` mode we
        // short-circuit pairing approval and mint a token immediately
        // — anyone who can reach the relay is trusted by definition.
        .route("/v1/auth/account/request", post(pairing::account_request))
        .route("/v2/auth/account/request", post(pairing::account_request_v2))
        // In-browser UI lives at /web. The HTML/JS bundle authenticates
        // through /v1/auth like any other client; the static routes
        // themselves don't carry the auth middleware.
        .route("/web", get(web::index))
        .route("/web/", get(web::index))
        .route("/web/index.html", get(web::index))
        .route("/web/app.js", get(web::app_js))
        .route("/web/style.css", get(web::style_css))
        .merge(authed)
        .fallback(unmatched_route)
        .with_state(state)
        .layer(middleware::from_fn(log_requests))
        .layer(socket_layer);

    let addr = format!("{}:{}", args.bind, args.port);
    let socket_addr: std::net::SocketAddr = addr.parse()?;
    match (args.tls_cert, args.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("load TLS cert={} key={}: {e}", cert_path.display(), key_path.display())
                })?;
            tracing::info!("happier-relay listening on https://{addr} (socket.io at /v1/updates/)");
            axum_server::bind_rustls(socket_addr, tls_config)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("happier-relay listening on http://{addr} (socket.io at /v1/updates/)");
            axum::serve(listener, app).await?;
        }
        _ => {
            anyhow::bail!("--tls-cert and --tls-key must be passed together");
        }
    }
    Ok(())
}

fn resolve_secret(input: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(var) = input.strip_prefix("env:") {
        let value = std::env::var(var)
            .map_err(|_| anyhow::anyhow!("env var {var} not set (or not visible to this process)"))?;
        Ok(value.into_bytes())
    } else {
        Ok(input.as_bytes().to_vec())
    }
}

/// Log every request after the response is built. INFO for 2xx/3xx
/// (one tidy line per call), WARN for 4xx/5xx so unexpected failures
/// surface in default-level output. Designed to make "what is the
/// mobile app actually hitting?" a one-glance question.
async fn log_requests(req: Request, next: Next) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status();
    let elapsed_ms = started.elapsed().as_millis();
    let path = uri.path();
    let query = uri.query().unwrap_or("");
    if status.is_success() || status.is_redirection() {
        tracing::info!(target: "happier_relay::http", "{method} {path}{}{query} -> {status} ({elapsed_ms} ms)", if query.is_empty() { "" } else { "?" });
    } else {
        tracing::warn!(target: "happier_relay::http", "{method} {path}{}{query} -> {status} ({elapsed_ms} ms)", if query.is_empty() { "" } else { "?" });
    }
    resp
}

/// `GET /v1/version` — happier mobile sync gate. Returns `{ok: true}`.
async fn version_probe() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// `GET /health` — connectivity gate the mobile homepage hits to
/// decide "can I reach this relay". Body must be the plain text
/// `ok` with status 200.
async fn health_probe() -> &'static str {
    "ok"
}

/// `GET /` — friendly landing page. Browsers, curl, and casual
/// liveness checks all hit this, so it's a small HTML doc instead of
/// a JSON discovery blob — easier to read at a glance. Clients that
/// need a machine-readable shape can pass `Accept: application/json`
/// and get a discovery payload back.
async fn root_discovery(headers: axum::http::HeaderMap) -> axum::response::Response {
    let wants_json = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|a| a.contains("application/json"));
    if wants_json {
        return Json(serde_json::json!({
            "ok": true,
            "name": "happier-relay",
            "version": env!("CARGO_PKG_VERSION"),
            "endpoints": ["/v1/auth", "/v1/auth/ping", "/v1/sessions", "/v1/kv", "/v1/artifacts", "/web"],
        }))
        .into_response();
    }
    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>happier-relay</title>
  <style>
    body {{ font-family: ui-monospace, Menlo, Consolas, monospace; background: #1a1a1a; color: #ddd; max-width: 720px; margin: 4em auto; padding: 0 1em; line-height: 1.6; }}
    h1 {{ color: #4ec9b0; font-size: 18px; }}
    a {{ color: #4ec9b0; }}
    code {{ background: #2a2a2a; padding: 1px 6px; border-radius: 3px; }}
    .dim {{ color: #888; }}
  </style>
</head>
<body>
  <h1>happier-relay · v{}</h1>
  <p>Single-tenant Rust relay for the <a href="https://github.com/happier-dev/happier">happier</a> wire protocol, bundled inside tab-atelier.</p>
  <p><a href="/web">Open the web UI →</a></p>
  <p class="dim">Endpoints: <code>/v1/auth</code> · <code>/v1/sessions</code> · <code>/v1/kv</code> · <code>/v1/artifacts</code> · <code>/v1/events</code> · <code>/v1/updates/</code> (socket.io)</p>
  <p class="dim">Pass <code>Accept: application/json</code> for a machine-readable discovery doc.</p>
</body>
</html>"#,
        env!("CARGO_PKG_VERSION")
    );
    axum::response::Html(body).into_response()
}

/// 404 fallback that logs the unmatched route at WARN. The mobile app
/// hitting an unimplemented endpoint shows up here.
async fn unmatched_route(req: Request) -> impl IntoResponse {
    let method = req.method().clone();
    let uri = req.uri().clone();
    tracing::warn!(target: "happier_relay::http", "UNMATCHED {method} {uri}");
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "route not implemented in happier-relay",
            "method": method.as_str(),
            "path": uri.path(),
        })),
    )
}

/// `GET /v2/sessions/{id}` dispatcher. tab-atelier tab artifacts are
/// surfaced as sessions to the mobile UI by `stubs::list_sessions_v2`,
/// but the existing `sessions::get_one` queries the `sessions` SQL
/// table — which never contains those ids. Try the tab path first;
/// fall back to the real session handler for ids that aren't tabs.
async fn dispatch_session_get_v2(
    axum::extract::State(state): axum::extract::State<state::AppState>,
    axum::Extension(user): axum::Extension<auth::UserId>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::Extension;
    let cloned_state = state.clone();
    let cloned_user = user.clone();
    let cloned_id = session_id.clone();
    match stubs::get_tab_session_v2(State(state), Extension(user), Path(session_id)).await {
        Ok(json) => json.into_response(),
        Err((status, body))
            if status == axum::http::StatusCode::NOT_FOUND
                && body.0.get("error").and_then(serde_json::Value::as_str) == Some("not-a-tab-artifact") =>
        {
            sessions::get_one(State(cloned_state), Extension(cloned_user), Path(cloned_id))
                .await
                .into_response()
        }
        Err((status, body)) => (status, body).into_response(),
    }
}

/// `GET /v1/sessions/{id}/messages` dispatcher. Same idea — synthesise
/// a single scrollback message for tab artifacts; otherwise delegate
/// to the SQL-backed `sessions::list_messages`.
async fn dispatch_session_messages_v1(
    axum::extract::State(state): axum::extract::State<state::AppState>,
    axum::Extension(user): axum::Extension<auth::UserId>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> axum::response::Response {
    use axum::extract::{Path, Query, State};
    use axum::response::IntoResponse;
    use axum::Extension;
    let raw = raw_query.unwrap_or_default();
    let tab_q: stubs::TabMessagesQuery = serde_urlencoded::from_str(&raw).unwrap_or(stubs::TabMessagesQuery {
        limit: None,
        after_seq: None,
        before_seq: None,
        role: None,
        roles: None,
        scope: None,
    });
    let cloned_state = state.clone();
    let cloned_user = user.clone();
    let cloned_id = session_id.clone();
    match stubs::list_tab_messages_v1(State(state), Extension(user), Path(session_id), Query(tab_q)).await {
        Ok(json) => json.into_response(),
        Err((status, body))
            if status == axum::http::StatusCode::NOT_FOUND
                && body.0.get("error").and_then(serde_json::Value::as_str) == Some("not-a-tab-artifact") =>
        {
            let session_q: sessions::MessagesQuery = serde_urlencoded::from_str(&raw).unwrap_or_default();
            sessions::list_messages(State(cloned_state), Extension(cloned_user), Path(cloned_id), Query(session_q))
                .await
                .into_response()
        }
        Err((status, body)) => (status, body).into_response(),
    }
}

/// Stable host identifier shown as the machine name in the mobile
/// UI. Persisted to `$XDG_CONFIG_HOME/tab-atelier/machine-id` on
/// first use so it survives restarts. Falls back to a literal
/// `"tab-atelier"` if neither HOSTNAME nor the file is usable —
/// the relay should still come up.
fn resolve_machine_id() -> String {
    let dir = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    let path = dir.join("tab-atelier").join("machine-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "host".to_string())
        });
    let id = format!("tab-atelier@{}", sanitize_machine_id(&hostname));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    id
}

fn sanitize_machine_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(64)
        .collect()
}

fn default_db_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("happier-relay").join("db.sqlite")
}
