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

mod auth;
mod db;
mod jwt;
mod sessions;
mod socket;
mod state;

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

    let state = state::AppState {
        db: pool,
        jwt_secret: Arc::new(secret),
        owner_pubkey_hex: args.owner_pubkey.map(|s| s.to_lowercase()),
    };

    // socket.io v4 — registered with the same AppState so the connect
    // handler can verify the JWT against our shared secret. socketioxide
    // returns a Tower layer we mount on the axum router below.
    let (socket_layer, io) = SocketIo::builder().with_state(state.clone()).build_layer();
    io.ns("/", socket::on_connect);

    // Authed routes get the middleware; public ones (just /v1/auth) don't.
    let authed = Router::new()
        .route("/v1/auth/ping", get(auth::ping_handler))
        .route("/v1/sessions", post(sessions::create).get(sessions::list_all))
        .route("/v1/sessions/{id}", axum_delete(sessions::delete))
        .route("/v2/sessions/{id}", get(sessions::get_one).patch(sessions::patch))
        .route("/v1/sessions/{id}/messages", get(sessions::list_messages))
        .route("/v2/sessions/{id}/messages", post(sessions::post_message))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        .route("/v1/auth", post(auth::auth_handler))
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
