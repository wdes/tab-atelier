// SPDX-License-Identifier: MPL-2.0

// Integration test crate — unwrap/expect are idiomatic here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests of the relay backend: spawn the real `catbus-agent`
//! binary in `--no-tui` mode, point it at a mock *relay* on localhost,
//! drive a prompt through the UNIX socket, and assert on what reached
//! the relay.
//!
//! The distinction from `openai_mock.rs` matters. These tests pin the
//! whole point of moving the login onto the proxy:
//!
//! * the request goes to `/relay/anthropic/v1/messages`, so the client
//!   is talking to a relay and not to `api.anthropic.com`;
//! * the relay token travels in `x-api-key`, and **no** `authorization`
//!   header is sent — the client has no subscription credential to leak;
//! * no local Claude credential is read or required.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A Messages reply that ends the turn immediately, in the shape the
/// Anthropic Messages API returns one.
const FINAL_ROUND: &str = r#"{
    "id": "msg_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-sonnet-4-6",
    "content": [{ "type": "text", "text": "hi from the relay" }],
    "stop_reason": "end_turn",
    "usage": { "input_tokens": 5, "output_tokens": 4 }
}"#;

const RELAY_TOKEN: &str = "tap_integration_test_token";

/// Serve one canned `(status line, JSON body)` per connection, in order.
/// Each raw request (headers + body) is pushed through the returned
/// channel for the test to assert on.
fn spawn_mock_relay(responses: Vec<(&'static str, &'static str)>) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status_line, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let raw = read_http_request(&mut stream);
            tx.send(raw).unwrap();
            let resp = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });
    (port, rx)
}

/// Read one HTTP/1.1 request (headers + `Content-Length` body).
fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed mid-request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
        .lines()
        .find_map(|l| {
            let (key, value) = l.split_once(':')?;
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Stop the agent when the test ends, pass or fail. SIGTERM first — the
/// agent exits cleanly on it (`socket.rs` installs a handler), and a clean
/// exit is what lets a coverage-instrumented binary flush its profile.
/// SIGKILL only if it hasn't exited within 5 s.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-TERM", &self.0.id().to_string()]).status();
        for _ in 0..100 {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The agent binary with a hermetic environment: an empty HOME (so no
/// stray `preferences.json` is picked up) and no relay/OpenAI/XDG vars
/// inherited from whoever ran the tests.
fn agent_command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catbus-agent"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CATBUS_RELAY_URL")
        .env_remove("CATBUS_RELAY_TOKEN")
        .env_remove("CATBUS_PREFERENCES")
        .env_remove("CATBUS_OPENAI_URL")
        .env_remove("CATBUS_OPENAI_TOKEN")
        .env_remove("CATBUS_OPENAI_MODEL")
        .env_remove("INFOMANIAK_PRODUCT_ID")
        .env_remove("INFOMANIAK_API_TOKEN");
    cmd
}

/// Spawn the agent, wait for its socket, and return the child (kept alive
/// by the caller) with a connected reader/writer pair. `configure` gets
/// the command so each test can add its own flags and env.
fn spawn_agent(
    home: &Path,
    socket: &Path,
    configure: impl FnOnce(&mut Command),
) -> (KillOnDrop, BufReader<UnixStream>, UnixStream) {
    let mut cmd = agent_command(home);
    cmd.args([
        "--new-session",
        "--cwd",
        home.to_str().unwrap(),
        "--socket",
        socket.to_str().unwrap(),
        "--no-tui",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    configure(&mut cmd);
    let child = KillOnDrop(cmd.spawn().unwrap());

    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        if let Ok(s) = UnixStream::connect(socket) {
            break s;
        }
        assert!(Instant::now() < deadline, "agent socket never appeared");
        std::thread::sleep(Duration::from_millis(50));
    };
    stream.set_read_timeout(Some(Duration::from_mins(1))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let started: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(started["kind"], "started");
    (child, reader, stream)
}

/// The common case: relay address and token given as flags.
fn spawn_agent_at(home: &Path, socket: &Path, port: u16) -> (KillOnDrop, BufReader<UnixStream>, UnixStream) {
    spawn_agent(home, socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    })
}

fn send_prompt(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, text: &str) -> serde_json::Value {
    let req = serde_json::json!({ "kind": "prompt", "text": text });
    stream.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn body_of(raw_request: &str) -> serde_json::Value {
    let body = raw_request.split("\r\n\r\n").nth(1).unwrap();
    serde_json::from_str(body).unwrap()
}

/// The headline test: a full "hi" round trip through the relay.
#[test]
fn a_prompt_round_trips_through_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    assert_eq!(reply["text"], "hi from the relay");

    let raw = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let lower = raw.to_lowercase();
    assert!(
        raw.starts_with("POST /relay/anthropic/v1/messages "),
        "must post to the relay's messages path, got: {raw}"
    );
    // The relay token is the only credential this process has.
    assert!(
        lower.contains(&format!("x-api-key: {}", RELAY_TOKEN.to_lowercase())),
        "missing relay token as x-api-key:\n{raw}"
    );
    // If either of these ever comes back, a subscription credential has
    // leaked into the client and the split has been undone.
    assert!(
        !lower.contains("authorization:"),
        "client must not send Authorization:\n{raw}"
    );
    assert!(
        !lower.contains("anthropic-beta:"),
        "the relay owns the beta flags, not the client:\n{raw}"
    );

    let body = body_of(&raw);
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hi");
    // The upstream demands the Claude Code identifier in the first system
    // block, and the relay forwards system blocks untouched — so sending
    // it is still the client's job and must survive the move.
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("You are Claude Code"),
        "system[0] must be the Claude Code identifier, got: {}",
        body["system"][0]
    );
    let tools = body["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["name"] == "Read"),
        "Anthropic-shaped tool spec missing"
    );
}

#[test]
fn a_relay_error_is_reported_over_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_relay(vec![(
        "HTTP/1.1 502 Bad Gateway",
        r#"{"type":"error","error":{"type":"api_error","message":"upstream is down"}}"#,
    )]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "error", "unexpected reply: {reply}");
    let text = reply["message"].as_str().unwrap();
    assert!(text.contains("upstream is down"), "error text: {text}");
}

#[test]
fn a_url_without_a_token_is_refused_with_the_preferences_path() {
    // A URL alone is a plausible half-configuration; the failure should
    // name the file to fix rather than looking like a network problem.
    let dir = tempfile::tempdir().unwrap();
    let out = agent_command(dir.path())
        .args([
            "--new-session",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--socket",
            dir.path().join("agent.sock").to_str().unwrap(),
            "--relay-url",
            "https://relay.example",
            "--print-socket",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a URL with no token must not start");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("preferences.json"), "stderr: {stderr}");
}

#[test]
fn the_cloudflare_access_pair_comes_from_preferences() {
    // A relay published through Cloudflare Access needs the service token,
    // and it must travel alongside the relay token. The URL is overridden
    // so the token and the Access pair have to come from the file.
    let dir = tempfile::tempdir().unwrap();
    let prefs = dir.path().join("preferences.json");
    std::fs::write(
        &prefs,
        r#"{
            "relay_endpoint_id": "relay-id",
            "remote_endpoints": [{
                "id": "relay-id",
                "url": "https://relay.example",
                "relay_token": "tap_from_prefs",
                "cf_access_client_id": "cf-id.access",
                "cf_access_client_secret": "cf-secret"
            }]
        }"#,
    )
    .unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.env("CATBUS_PREFERENCES", &prefs)
            .args(["--relay-url", &format!("http://127.0.0.1:{port}")]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let raw = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let lower = raw.to_lowercase();
    assert!(
        lower.contains("x-api-key: tap_from_prefs"),
        "token should come from preferences:\n{raw}"
    );
    assert!(
        lower.contains("cf-access-client-id: cf-id.access"),
        "missing Access id:\n{raw}"
    );
    assert!(
        lower.contains("cf-access-client-secret: cf-secret"),
        "missing Access secret:\n{raw}"
    );
}
