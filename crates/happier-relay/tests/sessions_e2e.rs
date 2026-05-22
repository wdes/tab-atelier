// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sessions + messages end-to-end. Creates a session, exercises the
//! optimistic CAS path on PATCH, posts a message with an
//! Idempotency-Key, then lists messages.

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
        if client.get(format!("http://127.0.0.1:{port}/__ping")).send().await.is_ok() {
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
    resp.json::<serde_json::Value>().await.unwrap()["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn session_crud_and_cas() {
    let port = free_port();
    let tmpdir = tempfile_dir("sessions_crud");
    let mut child = spawn_relay(port, "sessions-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();

    // CREATE
    let create = http
        .post(format!("{url}/v1/sessions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "tag": "device-1", "metadata": "cipher-v0" }))
        .send()
        .await
        .expect("create");
    assert_eq!(create.status(), 200, "create should succeed");
    let body: serde_json::Value = create.json().await.unwrap();
    let session_id = body["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["session"]["metadataVersion"], serde_json::Value::Number(1.into()));

    // PATCH with the right expectedVersion succeeds.
    let ok_patch = http
        .patch(format!("{url}/v2/sessions/{session_id}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "metadata": { "ciphertext": "cipher-v1", "expectedVersion": 1 }
        }))
        .send()
        .await
        .expect("patch");
    assert_eq!(ok_patch.status(), 200);
    let ok_body: serde_json::Value = ok_patch.json().await.unwrap();
    assert_eq!(ok_body["success"], serde_json::Value::Bool(true));
    assert_eq!(ok_body["metadata"]["version"], serde_json::Value::Number(2.into()));

    // PATCH with a stale expectedVersion surfaces the current state.
    let stale = http
        .patch(format!("{url}/v2/sessions/{session_id}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "metadata": { "ciphertext": "cipher-v?", "expectedVersion": 1 }
        }))
        .send()
        .await
        .expect("stale patch");
    assert_eq!(stale.status(), 200);
    let stale_body: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(stale_body["success"], serde_json::Value::Bool(false));
    assert_eq!(stale_body["error"], serde_json::Value::String("version-mismatch".into()));
    assert_eq!(stale_body["metadata"]["version"], serde_json::Value::Number(2.into()));
    assert_eq!(stale_body["metadata"]["value"], serde_json::Value::String("cipher-v1".into()));

    // GET single
    let one = http
        .get(format!("{url}/v2/sessions/{session_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("get");
    assert_eq!(one.status(), 200);
    let one_body: serde_json::Value = one.json().await.unwrap();
    assert_eq!(one_body["session"]["metadata"], serde_json::Value::String("cipher-v1".into()));

    // LIST
    let list = http
        .get(format!("{url}/v1/sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("list");
    let list_body: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list_body["sessions"].as_array().unwrap().len(), 1);

    // DELETE (soft) → re-list shows nothing.
    let del = http
        .delete(format!("{url}/v1/sessions/{session_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status(), 200);
    let list2 = http
        .get(format!("{url}/v1/sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("list after delete");
    assert_eq!(list2.json::<serde_json::Value>().await.unwrap()["sessions"].as_array().unwrap().len(), 0);

    let _ = child.kill().await;
}

#[tokio::test]
async fn message_post_and_idempotency() {
    let port = free_port();
    let tmpdir = tempfile_dir("sessions_messages");
    let mut child = spawn_relay(port, "messages-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();

    let session_id = http
        .post(format!("{url}/v1/sessions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "tag": "t", "metadata": "m" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // First POST writes.
    let first = http
        .post(format!("{url}/v2/sessions/{session_id}/messages"))
        .bearer_auth(&token)
        .header("Idempotency-Key", "abc-123")
        .json(&serde_json::json!({ "ciphertext": "msg-1" }))
        .send()
        .await
        .unwrap();
    let first_body: serde_json::Value = first.json().await.unwrap();
    assert_eq!(first_body["didWrite"], serde_json::Value::Bool(true));
    assert_eq!(first_body["message"]["seq"], serde_json::Value::Number(1.into()));

    // Second POST with the same Idempotency-Key dedupes.
    let second = http
        .post(format!("{url}/v2/sessions/{session_id}/messages"))
        .bearer_auth(&token)
        .header("Idempotency-Key", "abc-123")
        .json(&serde_json::json!({ "ciphertext": "msg-1-retry" }))
        .send()
        .await
        .unwrap();
    let second_body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second_body["didWrite"], serde_json::Value::Bool(false));
    assert_eq!(second_body["message"]["seq"], serde_json::Value::Number(1.into()));

    // Add a few more messages.
    for i in 2..5 {
        http.post(format!("{url}/v2/sessions/{session_id}/messages"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "ciphertext": format!("msg-{i}") }))
            .send()
            .await
            .unwrap();
    }

    // LIST messages
    let list = http
        .get(format!("{url}/v1/sessions/{session_id}/messages?afterSeq=0"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list.json().await.unwrap();
    let msgs = list_body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["content"], serde_json::Value::String("msg-1".into()));
    assert_eq!(msgs[3]["content"], serde_json::Value::String("msg-4".into()));

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
