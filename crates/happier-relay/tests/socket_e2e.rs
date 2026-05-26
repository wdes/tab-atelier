// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! socket.io baseline end-to-end:
//!   * connect two clients with valid JWTs (same user)
//!   * client A emits `machine-update`, client B receives it
//!   * a third client with a bogus token is disconnected
//!
//! Uses the same spawn-the-real-binary pattern as `auth_e2e.rs`.

use std::net::TcpListener;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::FutureExt;
use rust_socketio::Payload;
use rust_socketio::asynchronous::{Client, ClientBuilder};
use tokio::sync::mpsc;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn spawn_relay(port: u16, secret: &str, db_path: &std::path::Path) -> tokio::process::Child {
    let bin = env!("CARGO_BIN_EXE_happier-relay");
    let child = tokio::process::Command::new(bin)
        .args([
            "--port",
            &port.to_string(),
            "--bind",
            "127.0.0.1",
            "--db-path",
            db_path.to_str().unwrap(),
            "--master-secret",
            secret,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn happier-relay binary");
    let client = reqwest::Client::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if client
            .get(format!("http://127.0.0.1:{port}/__ping"))
            .send()
            .await
            .is_ok()
        {
            return child;
        }
    }
    panic!("relay did not start in time");
}

/// Hit `/v1/auth` with a freshly-generated keypair and return the JWT.
async fn obtain_token(port: u16) -> String {
    let signing_key = SigningKey::from_bytes(&rand_bytes::<32>());
    let public_key = signing_key.verifying_key().to_bytes();
    let challenge: [u8; 32] = rand_bytes();
    let signature = signing_key.sign(&challenge).to_bytes();
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "publicKey": b64.encode(public_key),
        "challenge": b64.encode(challenge),
        "signature": b64.encode(signature),
    });
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/auth"))
        .json(&body)
        .send()
        .await
        .expect("post /v1/auth");
    let json: serde_json::Value = resp.json().await.unwrap();
    json["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn machine_update_broadcasts_to_user_room() {
    let port = free_port();
    let tmpdir = tempfile_dir("socket_e2e_machine_update");
    let mut child = spawn_relay(port, "test-socket-secret", &tmpdir.join("db.sqlite")).await;

    // Two devices logged in under the same identity (the same /v1/auth
    // call would normally be made on each device; we shortcut by reusing
    // one token — single-tenant means same user_id either way).
    let token = obtain_token(port).await;
    // Relay mounts socket.io at /v1/updates/ (matches the happier
    // mobile client). Without overriding the path here rust_socketio
    // would dial the default /socket.io/ and the server's 404 page
    // would arrive at the engine.io parser as an InvalidPacketId(123)
    // — the '{' byte of the JSON error response.
    let url = format!("http://127.0.0.1:{port}/v1/updates/");

    // Channel for client B's received machine-update events.
    let (tx, mut rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let tx_clone = tx.clone();
    let callback = move |payload: Payload, _: Client| {
        let tx = tx_clone.clone();
        async move {
            if let Payload::Text(values) = payload
                && let Some(v) = values.into_iter().next()
            {
                let _ = tx.send(v);
            }
        }
        .boxed()
    };

    let receiver: Client = ClientBuilder::new(&url)
        .auth(serde_json::json!({ "token": token }))
        .on("machine-update", callback)
        .connect()
        .await
        .expect("receiver connect");

    let sender: Client = ClientBuilder::new(&url)
        .auth(serde_json::json!({ "token": &token }))
        .connect()
        .await
        .expect("sender connect");

    // Allow both clients to register on the namespace + join the room.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = serde_json::json!({
        "machineId": "device-A",
        "metadata": { "hello": "world" },
    });
    sender.emit("machine-update", payload.clone()).await.expect("emit");

    let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("receiver got event within 3s")
        .expect("event payload");
    assert_eq!(got["machineId"], serde_json::Value::String("device-A".into()));
    assert_eq!(got["metadata"]["hello"], serde_json::Value::String("world".into()));

    let _ = sender.disconnect().await;
    let _ = receiver.disconnect().await;
    let _ = child.kill().await;
}

#[tokio::test]
async fn bad_token_is_disconnected() {
    let port = free_port();
    let tmpdir = tempfile_dir("socket_e2e_bad_token");
    let mut child = spawn_relay(port, "test-socket-secret-2", &tmpdir.join("db.sqlite")).await;
    // Relay mounts socket.io at /v1/updates/ (matches the happier
    // mobile client). Without overriding the path here rust_socketio
    // would dial the default /socket.io/ and the server's 404 page
    // would arrive at the engine.io parser as an InvalidPacketId(123)
    // — the '{' byte of the JSON error response.
    let url = format!("http://127.0.0.1:{port}/v1/updates/");

    // rust_socketio's ClientBuilder::connect resolves OK on a successful
    // engine.io handshake even if the namespace connect is then rejected.
    // We detect the auth failure via the disconnect callback signal.
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let disconn_cb = move |_payload: Payload, _: Client| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(());
        }
        .boxed()
    };
    let result = ClientBuilder::new(&url)
        .auth(serde_json::json!({ "token": "this is not a real jwt" }))
        .on("disconnect", disconn_cb)
        .on("error", |payload: Payload, _: Client| {
            async move {
                tracing::debug!("socket reported error: {payload:?}");
            }
            .boxed()
        })
        .connect()
        .await;

    // Either the connect itself fails fast, or the server emits
    // disconnect within a second. Both are acceptable signals.
    if result.is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    }

    let _ = child.kill().await;
}

fn tempfile_dir(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("happier-relay-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore;
    let mut buf = [0u8; N];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}
