// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Artifacts end-to-end: create, get, partial-update with CAS, list,
//! and delete. Plus a socket.io subscriber that should observe an
//! `artifact-create` broadcast in the user's room.

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
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/auth"))
        .json(&body)
        .send()
        .await
        .expect("post /v1/auth")
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn artifact_lifecycle_and_fanout() {
    let port = free_port();
    let tmpdir = tempfile_dir("artifacts_lifecycle");
    let mut child = spawn_relay(port, "artifacts-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();
    let b64 = base64::engine::general_purpose::STANDARD;

    // socket.io subscriber. Captures any artifact-* event it sees.
    let (sock_tx, mut sock_rx) = mpsc::unbounded_channel::<(String, serde_json::Value)>();
    let make_cb = |tag: &'static str, tx: mpsc::UnboundedSender<(String, serde_json::Value)>| {
        move |payload: Payload, _: Client| {
            let tx = tx.clone();
            async move {
                if let Payload::Text(values) = payload
                    && let Some(v) = values.into_iter().next()
                {
                    let _ = tx.send((tag.to_string(), v));
                }
            }
            .boxed()
        }
    };
    let sub: Client = ClientBuilder::new(&url)
        .auth(serde_json::json!({ "token": &token }))
        .on("artifact-create", make_cb("create", sock_tx.clone()))
        .on("artifact-update", make_cb("update", sock_tx.clone()))
        .on("artifact-delete", make_cb("delete", sock_tx.clone()))
        .connect()
        .await
        .expect("subscriber connect");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let id = uuid::Uuid::new_v4().to_string();
    let create_body = serde_json::json!({
        "id": id,
        "header": b64.encode(b"header-v1"),
        "body": b64.encode(b"body-v1"),
        "dataEncryptionKey": b64.encode(b"key-bytes"),
    });
    let created = http
        .post(format!("{url}/v1/artifacts"))
        .bearer_auth(&token)
        .json(&create_body)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let resp_body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(resp_body["id"], serde_json::Value::String(id.clone()));
    assert_eq!(resp_body["headerVersion"], serde_json::Value::Number(1.into()));

    // GET single
    let one = http
        .get(format!("{url}/v1/artifacts/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(one.status(), 200);
    let one_body: serde_json::Value = one.json().await.unwrap();
    assert_eq!(one_body["body"], serde_json::Value::String(b64.encode(b"body-v1")));

    // Partial update: bump header only.
    let updated = http
        .post(format!("{url}/v1/artifacts/{id}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "header": b64.encode(b"header-v2"),
            "expectedHeaderVersion": 1,
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(updated["success"], serde_json::Value::Bool(true));
    assert_eq!(updated["headerVersion"], serde_json::Value::Number(2.into()));
    assert_eq!(updated["bodyVersion"], serde_json::Value::Number(1.into()));

    // Stale update fails and surfaces current state.
    let stale = http
        .post(format!("{url}/v1/artifacts/{id}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "header": b64.encode(b"header-v3"),
            "expectedHeaderVersion": 1,
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(stale["success"], serde_json::Value::Bool(false));
    assert_eq!(stale["currentHeaderVersion"], serde_json::Value::Number(2.into()));

    // LIST contains the artifact summary (no body).
    let list = http
        .get(format!("{url}/v1/artifacts"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert!(arr[0].get("body").is_none(), "list should not include body bytes");

    // DELETE.
    let del = http
        .delete(format!("{url}/v1/artifacts/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);

    // Wait briefly for the subscriber to collect at least one event.
    let mut seen: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while seen.len() < 3 && std::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), sock_rx.recv()).await {
            seen.insert(ev.0, ev.1);
        }
    }
    assert!(seen.contains_key("create"), "subscriber should see artifact-create");
    assert!(seen.contains_key("update"), "subscriber should see artifact-update");
    assert!(seen.contains_key("delete"), "subscriber should see artifact-delete");

    let _ = sub.disconnect().await;
    let _ = child.kill().await;
}

#[tokio::test]
async fn append_grows_body_with_cas() {
    let port = free_port();
    let tmpdir = tempfile_dir("artifacts_append");
    let mut child = spawn_relay(port, "append-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();
    let b64 = base64::engine::general_purpose::STANDARD;

    let id = uuid::Uuid::new_v4().to_string();
    http.post(format!("{url}/v1/artifacts"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "id": id,
            "header": b64.encode(b"h"),
            "body": b64.encode(b"$ ls\n"),
            "dataEncryptionKey": b64.encode([0_u8; 32]),
        }))
        .send()
        .await
        .unwrap();

    // Append once with the right version → body grows + version bumps.
    let resp = http
        .post(format!("{url}/v1/artifacts/{id}/append"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "expectedBodyVersion": 1, "suffix": b64.encode(b"foo bar baz\n") }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], serde_json::Value::Bool(true));
    assert_eq!(body["bodyVersion"], serde_json::Value::Number(2.into()));

    // GET shows the concatenated body.
    let got: serde_json::Value = http
        .get(format!("{url}/v1/artifacts/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        got["body"],
        serde_json::Value::String(b64.encode("$ ls\nfoo bar baz\n"))
    );

    // Stale append → version-mismatch with currentBody surfaced.
    let stale: serde_json::Value = http
        .post(format!("{url}/v1/artifacts/{id}/append"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "expectedBodyVersion": 1, "suffix": b64.encode(b"nope") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stale["success"], serde_json::Value::Bool(false));
    assert_eq!(stale["currentBodyVersion"], serde_json::Value::Number(2.into()));
    assert_eq!(
        stale["currentBody"],
        serde_json::Value::String(b64.encode("$ ls\nfoo bar baz\n"))
    );

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
