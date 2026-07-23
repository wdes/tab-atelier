// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Integration test crate — unwrap/expect are idiomatic here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests of the OpenAI-compatible backend: spawn the real
//! `catbus-agent` binary in `--no-tui` mode, point `--openai-url` at a
//! mock chat-completions server on localhost, drive a prompt through
//! the UNIX socket, and assert on both the HTTP requests the agent
//! sent and the reply it returned.
//!
//! The mock server answers each connection with one canned response
//! and `Connection: close`, so reqwest opens a fresh connection per
//! API round and the responses pair up with rounds in order.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Round 1: the model asks to Read `hello.txt` (empty content string
/// alongside the call, the way Grok / Mistral-family models emit it).
const TOOL_CALL_ROUND: &str = r#"{
    "id": "cmpl-1",
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "Read", "arguments": "{\"path\":\"hello.txt\"}" }
            }]
        },
        "finish_reason": "tool_calls"
    }],
    "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
}"#;

/// Round 2: the model answers with text and ends the turn.
const FINAL_ROUND: &str = r#"{
    "id": "cmpl-2",
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": { "role": "assistant", "content": "The file says: mock says hi" },
        "finish_reason": "stop"
    }],
    "usage": { "prompt_tokens": 20, "completion_tokens": 7 }
}"#;

/// Serve one canned `(status line, JSON body)` per connection, in
/// order. Each raw request (headers + body) is pushed through the
/// returned channel for the test to assert on.
fn spawn_mock_server(responses: Vec<(&'static str, &'static str)>) -> (u16, mpsc::Receiver<String>) {
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

/// Stop the agent when the test ends, pass or fail. SIGTERM first —
/// the agent exits cleanly on it (socket.rs installs a handler), and
/// a clean exit is what lets a coverage-instrumented binary flush its
/// profile to disk. SIGKILL only if it hasn't exited within 5 s.
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

fn spawn_agent(dir: &Path, socket: &Path, port: u16) -> KillOnDrop {
    let child = Command::new(env!("CARGO_BIN_EXE_catbus-agent"))
        .args([
            "--no-tui",
            "--new-session",
            "--cwd",
            dir.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--openai-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--openai-token",
            "test-token",
            "--openai-model",
            "test-model",
        ])
        // Sessions + transcripts land under $HOME/.claude — keep them
        // inside the tempdir so the test never touches the real one.
        .env("HOME", dir)
        // The agent must talk to the mock directly, never a proxy.
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env("NO_PROXY", "127.0.0.1,localhost")
        // A stray backend config in the test environment must not
        // override the flags above.
        .env_remove("CATBUS_OPENAI_URL")
        .env_remove("CATBUS_OPENAI_TOKEN")
        .env_remove("CATBUS_OPENAI_MODEL")
        .env_remove("INFOMANIAK_PRODUCT_ID")
        .env_remove("INFOMANIAK_API_TOKEN")
        // Coverage-instrumented builds default to ./default.profraw in
        // the child's cwd — the tempdir, which is deleted. Redirect to
        // the target tmpdir so grcov still picks the run up.
        .env(
            "LLVM_PROFILE_FILE",
            format!("{}/catbus-e2e-%p.profraw", env!("CARGO_TARGET_TMPDIR")),
        )
        .spawn()
        .unwrap();
    KillOnDrop(child)
}

/// Poll until the agent's socket accepts, then complete the
/// `{"kind":"started"}` handshake.
fn connect_socket(path: &Path) -> (BufReader<UnixStream>, UnixStream) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        if let Ok(s) = UnixStream::connect(path) {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "agent socket never appeared at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    stream.set_read_timeout(Some(Duration::from_mins(1))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let started: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(started["kind"], "started");
    (reader, stream)
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

#[test]
fn openai_backend_runs_a_tool_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "mock says hi\n").unwrap();
    let (port, rx) = spawn_mock_server(vec![
        ("HTTP/1.1 200 OK", TOOL_CALL_ROUND),
        ("HTTP/1.1 200 OK", FINAL_ROUND),
    ]);
    let socket = dir.path().join("agent.sock");
    let _agent = spawn_agent(dir.path(), &socket, port);

    let (mut reader, mut stream) = connect_socket(&socket);
    let reply = send_prompt(&mut stream, &mut reader, "read hello.txt please");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    assert_eq!(reply["text"], "The file says: mock says hi");

    // Round 1 on the wire: endpoint path, Bearer token, system-first
    // message list, converted tool specs.
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(first.starts_with("POST /v1/chat/completions "), "request line: {first}");
    assert!(
        first.to_lowercase().contains("authorization: bearer test-token"),
        "missing bearer token:\n{first}"
    );
    let req = body_of(&first);
    assert_eq!(req["model"], "test-model");
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][1]["role"], "user");
    assert_eq!(req["messages"][1]["content"], "read hello.txt please");
    let tools = req["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["function"]["name"] == "Read"),
        "Read tool spec missing"
    );

    // Round 2 must feed the tool output back as a `tool` message tied
    // to the call id, preceded by the assistant tool-call turn.
    let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let req = body_of(&second);
    let messages = req["messages"].as_array().unwrap();
    let assistant = &messages[messages.len() - 2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let tool_msg = &messages[messages.len() - 1];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    assert!(
        tool_msg["content"].as_str().unwrap().contains("mock says hi"),
        "tool result should carry the file contents: {tool_msg}"
    );
}

#[test]
fn api_error_is_reported_over_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_server(vec![(
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"mock exploded"}"#,
    )]);
    let socket = dir.path().join("agent.sock");
    let _agent = spawn_agent(dir.path(), &socket, port);

    let (mut reader, mut stream) = connect_socket(&socket);
    let reply = send_prompt(&mut stream, &mut reader, "hello?");
    assert_eq!(reply["kind"], "error", "unexpected reply: {reply}");
    let message = reply["message"].as_str().unwrap();
    assert!(
        message.contains("500") && message.contains("mock exploded"),
        "error should surface status and body: {message}"
    );
}
