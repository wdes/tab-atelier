// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! tab-input end-to-end: enqueue via POST, drain via GET, plus the
//! long-poll path (waiter wakes on the next POST instead of timing out).

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
async fn post_then_drain() {
    let port = free_port();
    let tmpdir = tempfile_dir("tab_input_drain");
    let mut child = spawn_relay(port, "tab-input-test-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let http = reqwest::Client::new();
    let b64 = base64::engine::general_purpose::STANDARD;

    // Enqueue two events.
    for keystroke in ["hello", "world"] {
        let resp = http
            .post(format!("{url}/v1/tab-input"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "tabName": "main", "bytes": b64.encode(keystroke) }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Immediate (non-waiting) drain since=0 returns both events.
    let drained = http
        .get(format!("{url}/v1/tab-input/pending?since=0"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let events = drained["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["tabName"], serde_json::Value::String("main".into()));
    assert_eq!(events[0]["bytes"], serde_json::Value::String(b64.encode("hello")));
    let high = drained["highestSeq"].as_i64().unwrap();
    assert!(high >= 2);

    // Another drain from the highestSeq returns nothing without a wait.
    let none = http
        .get(format!("{url}/v1/tab-input/pending?since={high}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(none["events"].as_array().unwrap().len(), 0);

    let _ = child.kill().await;
}

#[tokio::test]
async fn long_poll_wakes_on_post() {
    let port = free_port();
    let tmpdir = tempfile_dir("tab_input_longpoll");
    let mut child = spawn_relay(port, "tab-input-longpoll-secret", &tmpdir.join("db.sqlite")).await;
    let url = format!("http://127.0.0.1:{port}");
    let token = obtain_token(port).await;
    let b64 = base64::engine::general_purpose::STANDARD;

    // Start a long-poll with a generous wait, then fire a POST 300 ms in.
    let url_for_poll = url.clone();
    let token_for_poll = token.clone();
    let poll = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(format!("{url_for_poll}/v1/tab-input/pending?since=0&waitMs=5000"))
            .bearer_auth(&token_for_poll)
            .send()
            .await
            .expect("long poll send")
            .json::<serde_json::Value>()
            .await
            .expect("long poll json");
        resp
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    let post = reqwest::Client::new()
        .post(format!("{url}/v1/tab-input"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "tabName": "build", "bytes": b64.encode([0x03_u8]) }))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);

    let resp = tokio::time::timeout(Duration::from_secs(2), poll)
        .await
        .expect("long poll resolved within 2s")
        .expect("task panicked");
    let events = resp["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "long poll returned one event");
    assert_eq!(events[0]["tabName"], serde_json::Value::String("build".into()));

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
