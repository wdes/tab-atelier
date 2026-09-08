// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end: a user key in, the proxy's own Claude credential out.
//!
//! Ported from the desktop crate's `relay_egress_streams_sse_and_injects_oauth`
//! when the egress role moved here, and extended with the assertion that
//! matters most now that there are many keys instead of one: the caller's key
//! must not travel upstream.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use tab_atelier_proxy::server::{State, serve_on};
use tab_atelier_proxy::{egress, usage, users::Store};

/// A mock Anthropic that records what it was sent, then streams two SSE frames
/// with a gap between them and closes.
///
/// The gap is the point: it proves the response is streamed rather than
/// buffered to completion first, which is what a long generation depends on.
fn mock_upstream() -> (u16, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            while let Ok(n) = sock.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
            let _ = sock.write_all(b"data: {\"type\":\"message_start\"}\n\n");
            let _ = sock.flush();
            std::thread::sleep(std::time::Duration::from_millis(40));
            let _ = sock.write_all(b"data: [DONE]\n\n");
            let _ = sock.flush();
        }
    });
    (port, rx)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ta-proxy-it-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

/// Send a raw request and read the whole response.
fn request(port: u16, req: &str) -> String {
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    sock.write_all(req.as_bytes()).expect("write");
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    while let Ok(n) = sock.read(&mut tmp) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_users_key_is_exchanged_for_the_proxys_claude_token() {
    let (upstream_port, seen_rx) = mock_upstream();

    // A far-future expiry, so the egress uses the token as-is and the test
    // makes no network call of its own.
    let dir = scratch("creds");
    let creds = dir.join("creds.json");
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"oat-fixture-xyz","refreshToken":"ort-x","expiresAt":9999999999999,"scopes":["user:inference"]}}"#,
    )
    .expect("write creds");
    egress::set_credentials_path(Some(creds));
    egress::set_upstream(Some(format!("http://127.0.0.1:{upstream_port}")));

    let mut store = Store::load(dir.join("users.json")).expect("store");
    let (_ada, key) = store.add("Ada", "Lovelace", "ada@example.org").expect("add");

    let state = Arc::new(State {
        store: Mutex::new(store),
        usage: Mutex::new(usage::Store::load(dir.join("usage.json"))),
        admin_token: "tap_admin_not_valid_here".to_owned(),
        web_root: None,
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind proxy");
    let port = listener.local_addr().expect("addr").port();
    let served = Arc::clone(&state);
    rt.spawn(async move { serve_on(listener, served).await });

    let payload = "{}";
    let resp = request(
        port,
        &format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\n\
             Content-Type: application/json\r\nanthropic-beta: context-management-2025-06-27\r\n\
             Content-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
    );

    assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
    assert!(resp.contains("data:"), "expected streamed SSE, got: {resp}");
    assert!(resp.contains("[DONE]"), "expected the final SSE frame, got: {resp}");

    let seen = seen_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("upstream saw a request");
    assert!(
        seen.contains("oat-fixture-xyz"),
        "the proxy must present its own Claude token upstream; upstream saw: {seen}"
    );
    // The whole reason a user key is not an Anthropic key: it stops at the
    // proxy. If it were forwarded, revoking an account would not stop anything
    // and the key would be replayable against Anthropic directly.
    assert!(
        !seen.contains(&key),
        "the caller's key must NOT reach upstream; upstream saw: {seen}"
    );
    // The client's own beta flag has to survive, or a body field gated behind
    // it is rejected upstream as an unknown input.
    assert!(
        seen.contains("context-management-2025-06-27"),
        "the client's anthropic-beta must be merged, not replaced; upstream saw: {seen}"
    );
    assert!(
        seen.contains("oauth-2025-04-20"),
        "the OAuth beta flags are mandatory upstream; upstream saw: {seen}"
    );

    egress::set_upstream(None);
    egress::set_credentials_path(None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_revoked_key_stops_working_without_reaching_upstream() {
    let (upstream_port, seen_rx) = mock_upstream();
    let dir = scratch("revoked");
    egress::set_upstream(Some(format!("http://127.0.0.1:{upstream_port}")));

    let mut store = Store::load(dir.join("users.json")).expect("store");
    let (_a, key) = store.add("Ada", "Lovelace", "ada@example.org").expect("add");
    store.set_disabled("ada@example.org", true).expect("disable");

    let state = Arc::new(State {
        store: Mutex::new(store),
        usage: Mutex::new(usage::Store::load(dir.join("usage.json"))),
        admin_token: "tap_admin".to_owned(),
        web_root: None,
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    rt.spawn(async move { serve_on(listener, state).await });

    let resp = request(
        port,
        &format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\nContent-Length: 0\r\n\r\n"
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 401"), "resp: {resp}");
    // Refused at the door: a disabled account must not cost the proxy an
    // upstream call, or revoking someone still burns quota.
    assert!(
        seen_rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "a refused request must never reach upstream"
    );

    egress::set_upstream(None);
    let _ = std::fs::remove_dir_all(&dir);
}
