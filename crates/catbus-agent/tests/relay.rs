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
use std::path::{Path, PathBuf};
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
    // The content is a block array rather than the bare string `"hi"` because
    // the last real turn carries a cache breakpoint, and `cache_control`
    // attaches to a block. See `crate::cache`.
    assert_eq!(
        body["messages"][0]["content"][0]["text"], "hi",
        "the user's text must survive being wrapped for caching: {}",
        body["messages"][0]["content"]
    );
    // The prompt cache is the point of that wrapping, so assert it is really
    // on the wire rather than only in the unit tests.
    assert!(
        body["messages"][0]["content"][0]["cache_control"]["type"] == "ephemeral",
        "the last real turn must carry a cache breakpoint:\n{raw}"
    );
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
    // Both system blocks are static now, so both are worth caching. This is
    // the half of the fix that stops a gate or cwd change from invalidating
    // the whole conversation: nothing session-specific is in either block.
    let system = body["system"].as_array().expect("system is an array");
    assert_eq!(system.len(), 2, "both blocks are static, so there are two");
    for (i, block) in system.iter().enumerate() {
        assert!(
            block["cache_control"]["type"] == "ephemeral",
            "system[{i}] must carry a cache breakpoint:\n{raw}"
        );
        let text = block["text"].as_str().unwrap_or_default();
        assert!(
            !text.contains(&dir.path().display().to_string()),
            "system[{i}] must not carry the working directory — live state belongs \
             in the trailing env turn, or a change to it re-buys the prefix:\n{text}"
        );
    }
    // …and the live state really is there, last, where it cannot invalidate
    // anything before it.
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let last_text = last["content"].as_str().unwrap_or_default();
    assert!(
        last_text.contains("<env "),
        "the working directory and gate belong in a trailing turn, got: {}",
        last["content"]
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
    // Four responses, because a 502 is retryable and the agent now sends up to
    // four attempts before giving up — see `crate::retry`. One response would
    // leave the later connections refused, and the assertion below would then
    // be testing connection handling rather than error reporting.
    let bad = (
        "HTTP/1.1 502 Bad Gateway",
        r#"{"type":"error","error":{"type":"api_error","message":"upstream is down"}}"#,
    );
    let (port, _rx) = spawn_mock_relay(vec![bad, bad, bad, bad]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    // Slow, because the retry backoff is 1s + 2s + 4s. The socket timeout is
    // 30s, so it fits with room to spare.
    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "error", "unexpected reply: {reply}");
    let text = reply["message"].as_str().unwrap();
    assert!(
        text.contains("upstream is down"),
        "error text should survive the retries and name the cause: {text}"
    );
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

/// A value with every `cache_control` removed, recursively.
///
/// The marks move forward as a conversation grows — a breakpoint marks where a
/// prefix *ends* — so comparing two requests byte-for-byte would compare two
/// different placements that are both correct. Claude Code's captured bodies
/// show exactly that shape: one mark, on the last message, at 1562 of 1563.
///
/// Content must be identical; the marks are asserted separately, in
/// `the_breakpoint_stays_on_the_last_real_turn`.
fn strip_marks(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, val)| (k.clone(), strip_marks(val)))
                .collect(),
        ),
        serde_json::Value::Array(items) => items.iter().map(strip_marks).collect(),
        other => other.clone(),
    }
}

/// The text of a message, whether its content is a bare string or blocks.
fn text_of(message: &serde_json::Value) -> String {
    match &message["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Two requests across a permission-mode change must share their cached prefix.
///
/// This is the test for the defect this work started from. The working
/// directory and the mode used to live in a `system` block, *before* the static
/// instructions — so toggling the mode changed a system block and invalidated
/// that block and every message behind it. The whole conversation, re-bought to
/// change twenty bytes, at 50x the hit rate.
///
/// A test that only checked one request could not see it: every individual
/// request was well-formed and cacheable. The fault was only visible by
/// comparing two, which is what this does.
#[test]
fn toggling_the_gate_does_not_move_the_cached_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND), ("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    // One request in the default mode…
    let first = send_prompt(&mut stream, &mut reader, "first");
    assert_eq!(first["kind"], "done", "unexpected reply: {first}");
    let before = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());

    // …then change the mode, which is the thing that used to rewrite the prefix.
    stream.write_all(b"{\"kind\":\"set_plan_mode\",\"on\":true}\n").unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("gate = plan"), "unexpected reply: {line}");

    // …and ask again.
    let second = send_prompt(&mut stream, &mut reader, "second");
    assert_eq!(second["kind"], "done", "unexpected reply: {second}");
    let after = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());

    // The two constants of the prompt: system and tools lead the body, so a
    // byte of difference here invalidates everything behind it. Compared
    // serialized, because that is the form the provider hashes.
    assert_eq!(
        serde_json::to_string(&before["system"]).unwrap(),
        serde_json::to_string(&after["system"]).unwrap(),
        "changing the permission mode rewrote a system block, which invalidates \
         every message behind it:\nbefore: {}\nafter:  {}",
        before["system"],
        after["system"]
    );
    assert_eq!(
        serde_json::to_string(&before["tools"]).unwrap(),
        serde_json::to_string(&after["tools"]).unwrap(),
        "the tool array moved between two requests"
    );

    // The conversation grows; it does not change. Everything the first request
    // sent, apart from the moving cache markers and the state turn appended
    // after its last breakpoint, must be the exact beginning of the second.
    //
    // Compared with the markers stripped, and that is not a convenience. A
    // breakpoint marks where a prefix ENDS, so it moves forward as the
    // conversation grows: turn 1's last message carries a mark, and in turn 2
    // that same message is mid-history and the new last message carries it
    // instead. Claude Code's captured requests show exactly this — one mark, on
    // the last message, at 1562 of 1563 — and it reads its own prefix back at
    // 97–99%, so moving marks demonstrably do not break a cache. Asserting
    // byte-identity including the marker would be asserting a stricter rule
    // than the reference client follows.
    let real = |body: &serde_json::Value| -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(strip_marks)
            .collect();
        // Drop the trailing state turn: it is rendered per request, sits after
        // the last breakpoint, and is therefore never part of a cached prefix.
        if out
            .last()
            .is_some_and(|m| m["content"].as_str().is_some_and(|s| s.starts_with("<env ")))
        {
            out.pop();
        }
        out
    };
    let before_msgs = real(&before);
    let after_msgs = real(&after);
    assert!(
        after_msgs.len() > before_msgs.len(),
        "the second request should have grown: {} then {}",
        before_msgs.len(),
        after_msgs.len()
    );
    for (i, message) in before_msgs.iter().enumerate() {
        assert_eq!(
            message, &after_msgs[i],
            "message {i} differs between the two requests, so the provider cannot \
             read it back from cache"
        );
    }

    // The mark belongs on the last real message, and on the system blocks —
    // the placement the captured requests use. If a refactor ever moved it onto
    // the state turn, the prefix would be cached around a block that changes
    // every request, which is the failure this whole change is about.
    let marked: Vec<usize> = after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b.get("cache_control").is_some()))
        })
        .map(|(i, _)| i)
        .collect();
    let last_real = after_msgs.len() - 1;
    assert_eq!(
        marked,
        vec![last_real],
        "the message breakpoint must be on the last real turn and nowhere else; \
         got marks on {marked:?} of {} messages",
        after["messages"].as_array().unwrap().len()
    );
    assert!(
        after["messages"].as_array().unwrap()[last_real + 1]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("<env "),
        "the state turn must follow the last breakpoint, uncached"
    );

    // And the mode really did change, so the assertions above are about a
    // toggle that happened rather than one that silently did nothing.
    //
    // The text is read out of the block rather than searched for in
    // `Value::to_string()`. That serialization escapes the quotes — the needle
    // would have to be `gate=\"plan\"` — so a search for `gate="plan"` against
    // it never matches, and the negative assertion below would pass for
    // entirely the wrong reason. Reading the field is the honest comparison.
    assert!(
        !before_msgs.iter().any(|m| text_of(m).contains("gate=\"plan\"")),
        "the first request should be in the default mode"
    );
    let last = after["messages"].as_array().unwrap().last().unwrap();
    assert!(
        text_of(last).contains("gate=\"plan\""),
        "the second request's state turn should carry the new mode:\n{}",
        last["content"]
    );
}

/// A reply that asks for one tool call.
///
/// The call id is a parameter, and it matters: the transcript accumulates, so
/// by the third request the history holds every earlier `tool_result`. Sharing
/// one id across rounds makes a lookup by id ambiguous — it finds the oldest —
/// and the test then reads a stale result while believing it read the new one.
/// Distinct ids also mean the pairing this asserts is the real one.
fn tool_round(call_id: &str, name: &str, input: &str) -> String {
    format!(
        r#"{{
    "id": "msg_tool",
    "type": "message",
    "role": "assistant",
    "model": "claude-sonnet-4-6",
    "content": [{{ "type": "tool_use", "id": "{call_id}", "name": "{name}", "input": {input} }}],
    "stop_reason": "tool_use",
    "usage": {{ "input_tokens": 5, "output_tokens": 4 }}
}}"#
    )
}

/// The `tool_result` text a request carried back, by call id.
fn tool_result_of(body: &serde_json::Value, id: &str) -> String {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .and_then(|b| b["content"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("no tool_result for {id} in:\n{body}"))
}

/// A throwaway git repository with one commit and one untracked file.
fn git_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        vec!["commit", "-q", "--allow-empty", "-m", "first commit"],
    ] {
        let out = Command::new("git").args(&args).current_dir(&repo).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    std::fs::write(repo.join("untracked.txt"), "hello").unwrap();
    repo
}

/// A config with a git tool, a cargo tool and one taking an argument.
///
/// Written to a temp file rather than used from `examples/tools.json` so the
/// cargo entry can be `--version`: this test runs *inside* `cargo test`, which
/// holds the target-directory lock, so a nested `cargo test` or `cargo check`
/// would block until the outer run finished and the test would hang rather than
/// fail. A version query exercises the same exec path with no lock, and the
/// shipped example keeps the `cargo test` an operator actually wants.
fn tools_config(dir: &Path) -> PathBuf {
    let path = dir.join("tools.json");
    let config = serde_json::json!({
        "disable": ["Bash"],
        "add": [
            {
                "name": "GitStatus",
                "description": "Show the working tree status.",
                "schema": { "type": "object", "properties": {}, "required": [] },
                "argv": ["git", "status", "--short"],
                "timeout_secs": 15,
                "judged": false
            },
            {
                "name": "CargoVersion",
                "description": "Show the cargo version.",
                "schema": { "type": "object", "properties": {}, "required": [] },
                "argv": ["cargo", "--version"],
                "timeout_secs": 30,
                "judged": false
            },
            {
                "name": "GitLogLimit",
                "description": "Show the last N commits.",
                "schema": {
                    "type": "object",
                    "properties": { "max": { "type": "integer" } },
                    "required": ["max"]
                },
                "argv": ["git", "log", "--oneline", "--max-count={max}"],
                "timeout_secs": 15,
                "judged": false
            }
        ]
    });
    std::fs::write(&path, config.to_string()).unwrap();
    path
}

/// Configured tools really run, on the real binary, with real subprocesses.
///
/// The other tests here check what the agent *sends*. This one checks what it
/// *does*: only the model is mocked, while `dispatch`, `expand`,
/// `tokio::process::Command` and the socket loop are the shipping code. It also
/// asserts the configured tool array — `Bash` gone, three tools added — reached
/// the wire, and that the requests carry cache breakpoints.
#[test]
fn configured_tools_execute_for_real() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path());
    let config = tools_config(dir.path());
    let socket = dir.path().join("agent.sock");

    // The model calls each tool in turn, then stops. Each result comes back in
    // the *next* request, which is what the mock captures.
    let (port, rx) = spawn_mock_relay(vec![
        (
            "HTTP/1.1 200 OK",
            Box::leak(tool_round("call_git", "GitStatus", "{}").into_boxed_str()),
        ),
        (
            "HTTP/1.1 200 OK",
            Box::leak(tool_round("call_cargo", "CargoVersion", "{}").into_boxed_str()),
        ),
        (
            "HTTP/1.1 200 OK",
            Box::leak(tool_round("call_log", "GitLogLimit", r#"{"max": 1}"#).into_boxed_str()),
        ),
        ("HTTP/1.1 200 OK", FINAL_ROUND),
    ]);
    let (_agent, mut reader, mut stream) = spawn_agent(&repo, &socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            "tap_test_token",
            "--tools-config",
            config.to_str().unwrap(),
        ]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "check the repo");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    // Every request goes through the channel, including the first — which is
    // where the tool array belongs, before any tool has run.
    let first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());

    // The git tool printed the real `git status --short` output.
    let second = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let status = tool_result_of(&second, "call_git");
    assert!(
        status.contains("untracked.txt"),
        "GitStatus should have printed real git output, got: {status}"
    );

    // The cargo tool ran a real cargo.
    let third = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let cargo = tool_result_of(&third, "call_cargo");
    assert!(
        cargo.trim().starts_with("cargo "),
        "CargoVersion should have printed real `cargo --version` output, got: {cargo}"
    );

    // The argument was substituted as a whole argv element.
    let fourth = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let log = tool_result_of(&fourth, "call_log");
    assert!(
        log.contains("first commit"),
        "GitLogLimit should have run `git log --oneline --max-count=1`, got: {log}"
    );

    // The configured array reached the wire: Bash removed, three tools added.
    let names: Vec<&str> = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Bash"),
        "Bash was disabled but is still offered: {names:?}"
    );
    assert!(
        names.contains(&"Read"),
        "disabling Bash must not take the built-ins nobody asked to remove: {names:?}"
    );
    for wanted in ["GitStatus", "CargoVersion", "GitLogLimit"] {
        assert!(names.contains(&wanted), "{wanted} missing from {names:?}");
    }

    // Every system block carries a breakpoint, which is the half of the cache
    // fix that stops a mode toggle from invalidating the whole prompt.
    assert!(
        first["system"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b.get("cache_control").is_some()),
        "every system block should carry a breakpoint:\n{first}"
    );

    // And the growing transcript carries cache breakpoints, so it is being
    // cached rather than re-bought every turn.
    assert!(
        first["messages"].as_array().unwrap().iter().any(|m| m["content"]
            .as_array()
            .is_some_and(|c| c.iter().any(|b| b.get("cache_control").is_some()))),
        "no cache breakpoints on a real request:\n{first}"
    );
}
