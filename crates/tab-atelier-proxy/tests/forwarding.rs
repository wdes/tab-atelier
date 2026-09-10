// SPDX-License-Identifier: MPL-2.0

//! End-to-end: a user key in, the proxy's own Claude credential out.
//!
//! Ported from the desktop crate's `relay_egress_streams_sse_and_injects_oauth`
//! when the egress role moved here, and extended with the assertion that
//! matters most now that there are many keys instead of one: the caller's key
//! must not travel upstream.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// These tests set PROCESS-WIDE egress state (the credentials path, the
/// upstream override), so they cannot run at the same time. Cargo runs tests
/// in one binary on parallel threads by default, which without this makes them
/// clobber each other's fixtures in a way that looks like a proxy bug.
static EGRESS: Mutex<()> = Mutex::new(());

use tab_atelier_proxy::provider::{Auth, Class, Model, Provider, Registry, Wire};
use tab_atelier_proxy::server::{State, serve_on};
use tab_atelier_proxy::{account, egress, qos, usage, users::Store};

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
    let _serial = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let ada = store.add("Ada", "Lovelace", "ada@example.org").expect("add");
    // Accounts start with no keys — one is minted per place it is used.
    let (_k, key) = store.add_key(&ada.email, "laptop").expect("key");

    let state = Arc::new(State {
        store: Mutex::new(store),
        usage: Mutex::new(usage::Store::load(dir.join("usage"))),
        sched: Mutex::new(qos::Sched::new()),
        account: Mutex::new(account::Monitor::load(&dir)),
        inspect: Mutex::new(tab_atelier_proxy::inspect::Store::load(std::env::temp_dir())),
        wake: tokio::sync::Notify::new(),
        registry: tab_atelier_proxy::provider::Registry::default(),
        provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
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
             User-Agent: claude-cli/2.1.266 (external, cli)\r\nx-app: cli\r\n\
             x-claude-code-session-id: fdb378a6-aab8-4cd3-ba82-82c9a7248507\r\n\
             x-stainless-lang: js\r\nanthropic-dangerous-direct-browser-access: true\r\n\
             Cookie: session=secret-for-a-different-hop\r\n\
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
    // The client on the far side IS Claude Code, and Anthropic's OAuth path is
    // for Claude Code. The proxy used to rebuild the request from scratch,
    // which replaced that fingerprint with its own name and dropped the
    // session id — so every proxied call looked like an unknown client and no
    // support question about a session could be traced through.
    for expected in [
        "claude-cli/2.1.266",
        "x-app: cli",
        "fdb378a6-aab8-4cd3-ba82-82c9a7248507",
        // Enumerated by the SDK version rather than by us, so the prefix is
        // what keeps the allowlist from going stale on the client's upgrade.
        "x-stainless-lang",
        "anthropic-dangerous-direct-browser-access",
    ] {
        assert!(
            seen.contains(expected),
            "the client's Claude Code identity must reach upstream, missing {expected}; upstream saw: {seen}"
        );
    }
    assert!(
        !seen.contains("tab-atelier-proxy/"),
        "the proxy must not overwrite the client's User-Agent; upstream saw: {seen}"
    );
    // The forwarding list is an allowlist precisely so that a credential for a
    // different hop is not handed to Anthropic by accident.
    assert!(
        !seen.contains("secret-for-a-different-hop"),
        "a cookie is for another hop and must never be forwarded; upstream saw: {seen}"
    );

    egress::set_upstream(None);
    egress::set_credentials_path(None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_revoked_key_stops_working_without_reaching_upstream() {
    let _serial = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (upstream_port, seen_rx) = mock_upstream();
    let dir = scratch("revoked");
    egress::set_upstream(Some(format!("http://127.0.0.1:{upstream_port}")));

    let mut store = Store::load(dir.join("users.json")).expect("store");
    let a = store.add("Ada", "Lovelace", "ada@example.org").expect("add");
    // Accounts start with no keys — one is minted per place it is used.
    let (_k, key) = store.add_key(&a.email, "laptop").expect("key");
    store.set_disabled("ada@example.org", true).expect("disable");

    let state = Arc::new(State {
        store: Mutex::new(store),
        usage: Mutex::new(usage::Store::load(dir.join("usage"))),
        sched: Mutex::new(qos::Sched::new()),
        account: Mutex::new(account::Monitor::load(&dir)),
        inspect: Mutex::new(tab_atelier_proxy::inspect::Store::load(std::env::temp_dir())),
        wake: tokio::sync::Notify::new(),
        registry: tab_atelier_proxy::provider::Registry::default(),
        provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
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

/// The headline claim, end to end: when one provider refuses, the work moves
/// to another instead of stopping or getting worse.
#[test]
fn a_429_moves_the_next_request_to_another_provider() {
    let _serial = EGRESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Two upstreams. The first refuses with 429; the second answers.
    let (refuser, refuser_seen) = mock_status(429, "{\"error\":\"rate limited\"}");
    let (backup, backup_seen) = mock_status(200, "{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}");

    let dir = scratch("reroute");
    let creds = dir.join("creds.json");
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"oat-fixture","refreshToken":"r","expiresAt":9999999999999,"scopes":[]}}"#,
    )
    .expect("write creds");
    egress::set_credentials_path(Some(creds));

    let registry = Registry {
        providers: vec![
            Provider {
                id: "primary".to_owned(),
                wire: Wire::Anthropic,
                base_url: format!("http://127.0.0.1:{refuser}"),
                auth: Auth::ClaudeOauth,
                preference: 0,
                enabled: true,
                models: vec![Model {
                    id: "primary-balanced".to_owned(),
                    class: Class::Balanced,
                    relative_cost: 5,
                }],
            },
            Provider {
                id: "backup".to_owned(),
                wire: Wire::Anthropic,
                base_url: format!("http://127.0.0.1:{backup}"),
                auth: Auth::ClaudeOauth,
                preference: 1,
                enabled: true,
                models: vec![Model {
                    id: "backup-balanced".to_owned(),
                    class: Class::Balanced,
                    relative_cost: 6,
                }],
            },
        ],
    };

    let mut store = Store::load(dir.join("users.json")).expect("store");
    let a = store.add("Ada", "Lovelace", "ada@example.org").expect("add");
    // Accounts start with no keys — one is minted per place it is used.
    let (_k, key) = store.add_key(&a.email, "laptop").expect("key");
    let state = Arc::new(State {
        store: Mutex::new(store),
        usage: Mutex::new(usage::Store::load(dir.join("usage"))),
        sched: Mutex::new(qos::Sched::new()),
        account: Mutex::new(account::Monitor::load(&dir)),
        inspect: Mutex::new(tab_atelier_proxy::inspect::Store::load(std::env::temp_dir())),
        wake: tokio::sync::Notify::new(),
        registry,
        provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
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

    let call = || {
        let payload = r#"{"model":"primary-balanced","max_tokens":1,"messages":[]}"#;
        request(
            port,
            &format!(
                "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            ),
        )
    };

    // First call goes to the preferred provider and is refused. The 429 is
    // passed through rather than hidden — the client is entitled to it.
    let first = call();
    assert!(first.contains("429"), "first response: {first}");
    assert!(first.contains("x-tab-atelier-proxy-route: primary/"), "{first}");
    assert!(
        refuser_seen.recv_timeout(std::time::Duration::from_secs(3)).is_ok(),
        "the primary should have been tried"
    );

    // The refusal is remembered, so the NEXT call goes elsewhere. This is the
    // whole point: one provider's limit is not every provider's.
    let second = call();
    assert!(
        second.contains("x-tab-atelier-proxy-route: backup/backup-balanced"),
        "the second call should have moved to the backup: {second}"
    );
    assert!(second.starts_with("HTTP/1.1 200"), "{second}");
    assert!(
        backup_seen.recv_timeout(std::time::Duration::from_secs(3)).is_ok(),
        "the backup should have served it"
    );

    egress::set_credentials_path(None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Read a whole request — head plus the body its content-length declares.
///
/// Replying and closing after only the head turns the still-arriving body into
/// a connection reset, which surfaces as a 502 from the proxy and looks like a
/// proxy bug. (The same mistake was just fixed in tab-atelier's own test
/// server, in claude/fix-cli-edition-tests.)
fn drain_request(sock: &mut std::net::TcpStream) -> String {
    let mut req = Vec::new();
    let mut tmp = [0u8; 2048];
    let mut need: Option<usize> = None;
    loop {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => req.extend_from_slice(&tmp[..n]),
        }
        if need.is_none()
            && let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n")
        {
            let len = String::from_utf8_lossy(&req[..pos])
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            need = Some(pos + 4 + len);
        }
        if need.is_some_and(|n| req.len() >= n) {
            break;
        }
    }
    String::from_utf8_lossy(&req).into_owned()
}

/// A mock upstream that always answers with one status and body, reporting
/// each request it saw.
fn mock_status(status: u16, body: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let req = drain_request(&mut sock);
            let _ = tx.send(req);
            let _ = write!(
                sock,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.flush();
        }
    });
    (port, rx)
}
