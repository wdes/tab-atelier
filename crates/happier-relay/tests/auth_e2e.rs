// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end test that simulates what the happier mobile client does on
//! login: generate Ed25519 keypair, sign a 32-byte challenge, POST
//! `/v1/auth`, then GET `/v1/auth/ping` with the returned token.
//!
//! We spawn the real binary (not just the router) so this test exercises
//! the exact code path the CLI / mobile app would hit, including the
//! `SQLite` migration step.

use std::net::TcpListener;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

/// Find a free TCP port by binding 0 and reading the assigned port. The
/// listener is dropped before the relay binds, leaving a small race
/// window — acceptable for tests.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
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

    // Wait for the listener to come up. 50 × 100 ms = 5 s budget.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Anything that resolves the TCP connect is good enough; we'll
        // get a 404 or 400 since the path is bogus.
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

#[tokio::test]
async fn ed25519_challenge_response_round_trip() {
    let port = free_port();
    let tmpdir = tempfile_dir("auth_e2e_round_trip");
    let db_path = tmpdir.join("db.sqlite");
    let mut child = spawn_relay(port, "test-secret-1", &db_path).await;

    // Mobile-app simulation: generate keypair + 32-byte challenge.
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

    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/auth");
    let resp = client.post(&url).json(&body).send().await.expect("post /v1/auth");
    assert_eq!(resp.status(), 200, "auth should succeed");
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["success"], serde_json::Value::Bool(true));
    let token = json["token"].as_str().expect("token in response").to_string();

    // /v1/auth/ping with the returned bearer token must succeed.
    let ping_url = format!("http://127.0.0.1:{port}/v1/auth/ping");
    let ping = client
        .get(&ping_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("ping");
    assert_eq!(ping.status(), 200, "ping with valid bearer should succeed");
    let ping_json: serde_json::Value = ping.json().await.unwrap();
    assert_eq!(ping_json["ok"], serde_json::Value::Bool(true));

    // /v1/auth/ping without the bearer is rejected.
    let no_auth = client.get(&ping_url).send().await.expect("ping no auth");
    assert_eq!(no_auth.status(), 401);

    // /v1/auth/ping with a tampered bearer is rejected.
    let bad = client
        .get(&ping_url)
        .header("Authorization", format!("Bearer {token}x"))
        .send()
        .await
        .expect("ping bad");
    assert_eq!(bad.status(), 401);

    let _ = child.kill().await;
}

#[tokio::test]
async fn bogus_signature_rejected() {
    let port = free_port();
    let tmpdir = tempfile_dir("auth_e2e_bogus_sig");
    let db_path = tmpdir.join("db.sqlite");
    let mut child = spawn_relay(port, "test-secret-2", &db_path).await;

    let signing_key = SigningKey::from_bytes(&rand_bytes::<32>());
    let public_key = signing_key.verifying_key().to_bytes();
    let challenge: [u8; 32] = rand_bytes();
    let mut signature = signing_key.sign(&challenge).to_bytes();
    signature[0] ^= 0xff; // flip a bit

    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "publicKey": b64.encode(public_key),
        "challenge": b64.encode(challenge),
        "signature": b64.encode(signature),
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/auth"))
        .json(&body)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), 401);

    let _ = child.kill().await;
}

#[tokio::test]
async fn content_key_binding_round_trip() {
    let port = free_port();
    let tmpdir = tempfile_dir("auth_e2e_content_key");
    let db_path = tmpdir.join("db.sqlite");
    let mut child = spawn_relay(port, "test-secret-3", &db_path).await;

    let signing_key = SigningKey::from_bytes(&rand_bytes::<32>());
    let public_key = signing_key.verifying_key().to_bytes();
    let challenge: [u8; 32] = rand_bytes();
    let signature = signing_key.sign(&challenge).to_bytes();

    // 32-byte content (Curve25519 box) public key. Real clients would
    // derive this from a separate keypair; we just use random bytes —
    // the server only verifies the signed binding, not the key shape.
    let content_pk: [u8; 32] = rand_bytes();
    let mut payload = Vec::with_capacity(21 + 32);
    payload.extend_from_slice(b"Happy content key v1\0");
    payload.extend_from_slice(&content_pk);
    let content_sig = signing_key.sign(&payload).to_bytes();

    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "publicKey": b64.encode(public_key),
        "challenge": b64.encode(challenge),
        "signature": b64.encode(signature),
        "contentPublicKey": b64.encode(content_pk),
        "contentPublicKeySig": b64.encode(content_sig),
    });

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/auth"))
        .json(&body)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), 200, "auth with content-key binding should succeed");

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
