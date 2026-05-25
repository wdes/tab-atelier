// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `--shared-account` flag — two different keypairs auth and both end
//! up bound to the same account, so an artifact created by one is
//! visible to the other.

use std::net::TcpListener;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn spawn_relay_shared(port: u16, db_path: &std::path::Path) -> tokio::process::Child {
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
            "shared-account-secret",
            "--shared-account",
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

async fn authenticate(port: u16) -> String {
    let signing_key = SigningKey::from_bytes(&rand_bytes::<32>());
    let public_key = signing_key.verifying_key().to_bytes();
    let challenge: [u8; 32] = rand_bytes();
    let signature = signing_key.sign(&challenge).to_bytes();
    let b64 = base64::engine::general_purpose::STANDARD;
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/auth"))
        .json(&serde_json::json!({
            "publicKey": b64.encode(public_key),
            "challenge": b64.encode(challenge),
            "signature": b64.encode(signature),
        }))
        .send()
        .await
        .expect("auth")
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn two_devices_see_each_others_artifacts() {
    let port = free_port();
    let tmpdir = tempfile_dir("shared_account");
    let mut child = spawn_relay_shared(port, &tmpdir.join("db.sqlite")).await;
    let base = format!("http://127.0.0.1:{port}");
    let b64 = base64::engine::general_purpose::STANDARD;

    // Two independent keypairs auth.
    let token_a = authenticate(port).await;
    let token_b = authenticate(port).await;

    // Device A creates an artifact.
    let id = uuid::Uuid::new_v4().to_string();
    let create = reqwest::Client::new()
        .post(format!("{base}/v1/artifacts"))
        .bearer_auth(&token_a)
        .json(&serde_json::json!({
            "id": id,
            "header": b64.encode(b"header-from-A"),
            "body": b64.encode(b"body-from-A"),
            "dataEncryptionKey": b64.encode([0u8; 32]),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 200, "device A create");

    // Device B lists artifacts and sees the one A created.
    let list = reqwest::Client::new()
        .get(format!("{base}/v1/artifacts"))
        .bearer_auth(&token_b)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1, "device B should see device A's artifact");
    assert_eq!(arr[0]["id"], serde_json::Value::String(id));

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
