// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! KV store end-to-end. Covers create (version == -1), update with the
//! right version, version-mismatch surfacing, delete, list with prefix,
//! and bulk get.

use std::net::TcpListener;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

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
    resp.json::<serde_json::Value>().await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn kv_lifecycle() {
    let port = free_port();
    let tmpdir = tempfile_dir("kv_lifecycle");
    let mut child = spawn_relay(port, "kv-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();
    let b64 = base64::engine::general_purpose::STANDARD;

    // GET missing → 404
    let missing = http
        .get(format!("{url}/v1/kv/foo"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // CREATE (version: -1)
    let created = http
        .post(format!("{url}/v1/kv"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "mutations": [{ "key": "foo", "value": b64.encode(b"value-a"), "version": -1 }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let cbody: serde_json::Value = created.json().await.unwrap();
    assert_eq!(cbody["success"], serde_json::Value::Bool(true));
    assert_eq!(cbody["results"][0]["version"], serde_json::Value::Number(1.into()));

    // GET hits the row.
    let got = http
        .get(format!("{url}/v1/kv/foo"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let gbody: serde_json::Value = got.json().await.unwrap();
    assert_eq!(gbody["version"], serde_json::Value::Number(1.into()));
    assert_eq!(gbody["value"], serde_json::Value::String(b64.encode(b"value-a")));

    // UPDATE (right version)
    let updated = http
        .post(format!("{url}/v1/kv"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "mutations": [{ "key": "foo", "value": b64.encode(b"value-b"), "version": 1 }]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(updated["success"], serde_json::Value::Bool(true));
    assert_eq!(updated["results"][0]["version"], serde_json::Value::Number(2.into()));

    // Stale UPDATE (still version: 1) — should fail and surface state.
    let stale = http
        .post(format!("{url}/v1/kv"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "mutations": [{ "key": "foo", "value": b64.encode(b"value-c"), "version": 1 }]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(stale["success"], serde_json::Value::Bool(false));
    assert_eq!(
        stale["errors"][0]["error"],
        serde_json::Value::String("version-mismatch".into())
    );
    assert_eq!(stale["errors"][0]["version"], serde_json::Value::Number(2.into()));

    // Add a second key for LIST + BULK GET.
    http.post(format!("{url}/v1/kv"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "mutations": [{ "key": "bar", "value": b64.encode(b"value-bar"), "version": -1 }]
        }))
        .send()
        .await
        .unwrap();

    let listed = http
        .get(format!("{url}/v1/kv?prefix=fo"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["key"], serde_json::Value::String("foo".into()));

    let bulk = http
        .post(format!("{url}/v1/kv/bulk"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "keys": ["foo", "bar", "missing"] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let values = bulk["values"].as_array().unwrap();
    assert_eq!(values.len(), 2); // missing keys aren't in the response

    // DELETE via mutation with version: null
    let deleted = http
        .post(format!("{url}/v1/kv"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "mutations": [{ "key": "foo", "value": null, "version": 2 }]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(deleted["success"], serde_json::Value::Bool(true));
    let after = http
        .get(format!("{url}/v1/kv/foo"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 404);

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
