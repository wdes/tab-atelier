// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! happier-relay — single-tenant Rust server speaking the happier
//! (`happier-dev/happier`) wire protocol. Phase-1 spike: `/v1/auth`
//! and `/v1/auth/ping` only.

#![allow(clippy::module_name_repetitions)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::{middleware, routing::{delete as axum_delete, get, post}, Router};
use clap::Parser;
use socketioxide::SocketIo;
use tracing_subscriber::EnvFilter;

mod artifacts;
mod auth;
mod db;
mod jwt;
mod kv;
mod sessions;
mod socket;
mod sse;
mod state;
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
}

#[tokio::main]
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
    let (socket_layer, io) = SocketIo::builder().build_layer();
    let (broadcast_tx, _broadcast_rx_initial) = tokio::sync::broadcast::channel::<state::BroadcastMsg>(256);

    let state = state::AppState {
        db: pool,
        jwt_secret: Arc::new(secret),
        owner_pubkey_hex: args.owner_pubkey.map(|s| s.to_lowercase()),
        shared_account: args.shared_account,
        broadcast_tx: broadcast_tx.clone(),
        input_notifier: tab_input::InputNotifier::default(),
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
        .route("/v2/sessions/{id}", get(sessions::get_one).patch(sessions::patch))
        .route("/v1/sessions/{id}/messages", get(sessions::list_messages))
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
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        .route("/v1/auth", post(auth::auth_handler))
        // In-browser UI lives at /web. The HTML/JS bundle authenticates
        // through /v1/auth like any other client; the static routes
        // themselves don't carry the auth middleware.
        .route("/web", get(web::index))
        .route("/web/", get(web::index))
        .route("/web/index.html", get(web::index))
        .route("/web/app.js", get(web::app_js))
        .route("/web/style.css", get(web::style_css))
        .merge(authed)
        .with_state(state)
        .layer(socket_layer);

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("happier-relay listening on http://{addr} (socket.io at /socket.io/)");
    axum::serve(listener, app).await?;
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

fn default_db_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("happier-relay").join("db.sqlite")
}
