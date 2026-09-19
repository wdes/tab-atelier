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
    spawn_mock_relay_owned(
        responses
            .into_iter()
            .map(|(status, body)| (status, body.to_owned()))
            .collect(),
    )
}

/// [`spawn_mock_relay`] for a body that has to be *built* rather than written
/// as a literal — a reply holding an escape, since a JSON string may not carry
/// a bare control byte (RFC 8259) and so the escape must be produced by the
/// serialiser.
fn spawn_mock_relay_owned(responses: Vec<(&'static str, String)>) -> (u16, mpsc::Receiver<String>) {
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
///
/// `NO_COLOR` and `CLICOLOR` are cleared for the same reason as the rest: they
/// steer reply formatting, and an operator running the suite inside a
/// `NO_COLOR` shell should not get different results from CI. A test that wants
/// them sets them for its own child.
fn agent_command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catbus-agent"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        // The state dir too: `Tasks` resolves its list through
        // `$XDG_STATE_HOME` before `$HOME`, so without this a test run from a
        // shell that sets it would write into the operator's real task lists.
        .env_remove("XDG_STATE_HOME")
        .env_remove("CATBUS_RELAY_URL")
        .env_remove("CATBUS_RELAY_TOKEN")
        .env_remove("CATBUS_PREFERENCES")
        .env_remove("CATBUS_OPENAI_URL")
        .env_remove("CATBUS_OPENAI_TOKEN")
        .env_remove("CATBUS_OPENAI_MODEL")
        .env_remove("INFOMANIAK_PRODUCT_ID")
        .env_remove("INFOMANIAK_API_TOKEN")
        .env_remove("CATBUS_ANSI")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR");
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
    spawn_agent_in(home, home, socket, configure)
}

/// As [`spawn_agent`], but with the agent's working directory separate from its
/// HOME.
///
/// Two paths rather than one because `--cwd` cannot simply be passed twice: clap
/// rejects a repeated argument, so a test wanting a fixture tree distinct from
/// the tempdir holding `.claude` has to say so here. Kept as a delegating
/// variant rather than a parameter on [`spawn_agent`] so the common case — cwd
/// is where HOME is — stays a three-argument call.
fn spawn_agent_in(
    home: &Path,
    cwd: &Path,
    socket: &Path,
    configure: impl FnOnce(&mut Command),
) -> (KillOnDrop, BufReader<UnixStream>, UnixStream) {
    let mut cmd = agent_command(home);
    cmd.args([
        "--new-session",
        "--cwd",
        cwd.to_str().unwrap(),
        "--socket",
        socket.to_str().unwrap(),
        "--no-tui",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    configure(&mut cmd);
    let mut child = KillOnDrop(cmd.spawn().unwrap());

    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        if let Ok(s) = UnixStream::connect(socket) {
            break s;
        }
        // A start failure is almost always a bad flag or an unreadable config
        // file, and the agent explains it on stderr — which the harness would
        // otherwise discard, leaving "socket never appeared" as the only clue
        // and forcing a manual repro of whatever the test was doing.
        assert!(
            Instant::now() < deadline,
            "agent socket never appeared; it exited with:\n{}",
            stop_and_read_stderr(&mut child)
        );
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

/// Everything the child wrote to stderr, after stopping it.
///
/// Killed first so the read reaches EOF instead of blocking on a process that
/// never got far enough to close its own stderr.
fn stop_and_read_stderr(child: &mut KillOnDrop) -> String {
    let _ = child.0.kill();
    let _ = child.0.wait();
    let mut text = String::new();
    if let Some(mut err) = child.0.stderr.take() {
        let _ = err.read_to_string(&mut text);
    }
    if text.trim().is_empty() {
        "<no output>".to_owned()
    } else {
        text.trim().to_owned()
    }
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

/// A tool round that also says something, unlike [`tool_round`].
///
/// The distinction is not cosmetic: the loop's round cap behaves differently
/// depending on whether any prose was produced. With text it appends a warning
/// to what it has; with nothing but tool calls there is no answer to annotate,
/// so it returns an error instead. Testing the cap therefore needs a round that
/// talks *and* asks for a tool.
fn talking_tool_round(call_id: &str, name: &str, input: &str, text: &str) -> String {
    serde_json::json!({
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "text", "text": text },
            { "type": "tool_use", "id": call_id, "name": name, "input": serde_json::from_str::<serde_json::Value>(input).unwrap() }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 5, "output_tokens": 4 }
    })
    .to_string()
}

/// A throwaway git repository with one commit and one untracked file.
fn git_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        // Signing off explicitly. A developer machine commonly sets
        // `commit.gpgsign = true` for a hardware key, and `git config` here is
        // local but `commit.gpgsign` is not — so without this the commit stops
        // for a PIN prompt no test can answer and the failure reads as a
        // broken tool rather than an inherited preference.
        vec!["config", "commit.gpgsign", "false"],
        vec!["commit", "-q", "--no-gpg-sign", "--allow-empty", "-m", "first commit"],
    ] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(&repo)
            // Nothing above the repo: no operator hooks, no aliases, no
            // conditional includes. The repo's own `config` calls still apply,
            // since those are written into the work tree we just created.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
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

/// A reply that decorates its prose with SGR — what a model still sends when a
/// reader with no terminal is listening.
///
/// Serialised rather than written out as a literal, because a JSON string may
/// not contain a bare control byte (RFC 8259) and `serde_json` rejects one.
/// The `\x1b` below is an ordinary Rust escape in the *source* that becomes a
/// real ESC at compile time, and the serialiser then emits it in JSON's own
/// escaped form on the wire — so the client decodes a genuine escape while this
/// file never holds an invisible byte.
///
/// That distinction cost a round of debugging: the first version embedded the
/// escape directly, and a raw control character is invisible in a diff and easy
/// to mangle while editing, so the test failed as a *decode* error instead of
/// failing on the colour bug it was written for.
///
/// The judge's answer: a severity on its own, with no closing tag.
///
/// The judge is asked with `</severity>` as a stop sequence, so generation stops
/// *before* the closer is emitted and the real reply is a bare `<severity>N`.
/// Mimicking that matters: a mock that returned the tidy closed form would test
/// a shape the server never actually produces.
fn judge_verdict(severity: u8) -> String {
    serde_json::json!({
        "id": "msg_judge",
        "type": "message",
        "role": "assistant",
        "model": "deepseek-flash",
        "content": [{ "type": "text", "text": format!("<severity>{severity}") }],
        "stop_reason": "stop_sequence",
        "usage": { "input_tokens": 5, "output_tokens": 4 }
    })
    .to_string()
}

/// The literal `[0m` in the middle is deliberate: that is *text*, not an
/// escape, and must survive. A filter matching the visible shape would delete a
/// sentence like this one.
fn round_with_escapes() -> String {
    serde_json::json!({
        "id": "msg_esc",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{
            "type": "text",
            "text": "\x1b[1mMy tools\x1b[0m (a literal [0m stays) and \x1b[36mcyan\x1b[0m."
        }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 5, "output_tokens": 4 }
    })
    .to_string()
}

/// Every `.jsonl` under `dir`, found recursively.
fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// The agent's transcript on disk, as raw text.
///
/// Returned raw, not parsed, on purpose: the assertion is that no escape
/// survived, and `serde_json` writes a control byte as its six-character JSON
/// spelling rather than the byte itself. So the check has to look for *both*
/// forms, since neither alone would catch a transcript that kept an escape.
/// `HOME` is a tempdir here, so there is exactly one project directory with one
/// transcript — hence the exact-count assertion, which fails loudly rather than
/// silently reading the wrong file if that ever changes.
fn transcript_text(home: &Path) -> String {
    let projects = home.join(".claude").join("projects");
    let mut files = Vec::new();
    jsonl_files(&projects, &mut files);
    assert_eq!(files.len(), 1, "expected one transcript, found {files:?}");
    std::fs::read_to_string(&files[0]).unwrap()
}

/// The `--no-tui` default is the plain instruction, because the reader is
/// whatever is on the socket — often a phone or an API client with no terminal.
/// This is the regression for the reported bug: the session used to be told to
/// emit SGR unconditionally, so those readers showed `[36m` as literal text.
#[test]
fn a_reader_without_a_terminal_is_not_taught_escape_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    send_prompt(&mut stream, &mut reader, "hi");
    let body = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let instructions = body["system"][1]["text"].as_str().expect("instructions block");

    assert!(
        instructions.contains("Do NOT use markdown or ANSI escape sequences"),
        "a non-terminal reader must be told to avoid escapes:\n{instructions}"
    );
    assert!(
        !instructions.contains('\u{1b}'),
        "the plain instruction must not demonstrate escapes:\n{instructions:?}"
    );
    // Swapping the instruction text must not disturb the block the upstream
    // requires to come first.
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("You are Claude Code")
    );
}

/// `--ansi` opts back in, for a caller that knows its pipe renders escapes.
#[test]
fn ansi_is_opt_in_for_a_session_whose_stdout_is_not_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
            "--ansi",
        ]);
    });

    send_prompt(&mut stream, &mut reader, "hi");
    let body = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let instructions = body["system"][1]["text"].as_str().expect("instructions block");
    assert!(
        instructions.contains("terminal emulator"),
        "--ansi must select the terminal instruction:\n{instructions}"
    );
}

/// The instruction is a request, not a guarantee, so escapes are also filtered
/// on the way out — and the transcript is a *separate* reader that needs its
/// own filtering, because a reply reaches it as the model sent it rather than
/// through the socket path.
#[test]
fn escapes_are_stripped_from_the_reply_and_from_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_relay_owned(vec![("HTTP/1.1 200 OK", round_with_escapes())]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    let text = reply["text"].as_str().unwrap();
    assert_eq!(
        text, "My tools (a literal [0m stays) and cyan.",
        "escapes stripped, and text that merely looks like one kept"
    );
    assert!(!text.contains('\u{1b}'), "socket reply kept an escape: {text:?}");

    // Checked in both forms: `serde_json` writes a control byte as its
    // six-character JSON spelling, so a raw-byte check alone would pass on a
    // transcript that still holds an escape.
    let transcript = transcript_text(dir.path());
    assert!(
        !transcript.contains("\\u001b") && !transcript.contains('\u{1b}'),
        "transcript kept an escape:\n{transcript}"
    );
    assert!(
        transcript.contains("My tools (a literal [0m stays) and cyan."),
        "transcript should hold the stripped prose:\n{transcript}"
    );
}

/// The transcript is plain whichever sink the session has.
///
/// A terminal session may legitimately keep its colour in the answer, but the
/// transcript is shared and read by something that renders text — so the two
/// viewers differ on purpose. Pinned because the easy simplification — filtering
/// `resp.content` once and using it for both — silently puts escapes back in
/// front of the bubble renderer.
#[test]
fn the_transcript_stays_plain_even_when_the_reply_keeps_colour() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_relay_owned(vec![("HTTP/1.1 200 OK", round_with_escapes())]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
            "--ansi",
        ]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    let text = reply["text"].as_str().unwrap();
    assert!(
        text.contains('\u{1b}'),
        "--ansi means the terminal gets its colour: {text:?}"
    );

    let transcript = transcript_text(dir.path());
    assert!(
        !transcript.contains("\\u001b") && !transcript.contains('\u{1b}'),
        "the transcript is read by a renderer, so it stays plain:\n{transcript}"
    );
    assert!(
        transcript.contains("My tools (a literal [0m stays) and cyan."),
        "transcript should hold the stripped prose:\n{transcript}"
    );
}

/// An explicit `--ansi` outranks the ambient colour convention.
///
/// This is the one precedence step observable from out here: the harness always
/// runs `--no-tui` with a piped stdout, so a bare `NO_COLOR` can only *confirm*
/// the plain default and would pass either way. Setting the flag on top is the
/// case where the two sources disagree, and the flag has to win — otherwise
/// `NO_COLOR` inherited from a shell profile would make `--ansi` unusable.
///
/// The other combinations live in `ansi::allow_escapes`'s unit tests, which can
/// express "a terminal that `NO_COLOR` overrides" directly; no subprocess test
/// can, since a test's stdout is never a tty.
#[test]
fn an_explicit_ansi_flag_outranks_no_color() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.env("NO_COLOR", "1").args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
            "--ansi",
        ]);
    });

    send_prompt(&mut stream, &mut reader, "hi");
    let body = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let instructions = body["system"][1]["text"].as_str().expect("instructions block");
    assert!(
        instructions.contains("terminal emulator"),
        "--ansi must beat NO_COLOR, got:\n{instructions}"
    );
}

/// The same pair in the other order: `NO_COLOR` with no flag is plain.
///
/// Pinned even though the harness's own default already produces a plain
/// instruction, because it is the shape tab-atelier actually creates — its
/// `new_tab_env` sets `NO_COLOR=1` for agent-requested tabs — and a regression
/// that let escapes through when `NO_COLOR` is set would otherwise go unnoticed
/// here.
#[test]
fn an_agent_tab_marked_no_color_gets_the_plain_instruction() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.env("NO_COLOR", "1").args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    });

    send_prompt(&mut stream, &mut reader, "hi");
    let body = body_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let instructions = body["system"][1]["text"].as_str().expect("instructions block");
    assert!(
        instructions.contains("Do NOT use markdown or ANSI escape sequences"),
        "NO_COLOR must select the plain instruction:\n{instructions}"
    );
}

// ---------------------------------------------------------------------------
// The minimal tool set: Read + Write + FileTree, no shell.
//
// These drive the real binary against a mock relay, so they exercise the whole
// path a task takes: the specs that reach the wire, the dispatch of each call,
// and the result the model sees back. A unit test on `filetree::run` would miss
// whether the tool is registered at all, or whether a `Write` that a FileTree
// found a path for actually lands on disk.
// ---------------------------------------------------------------------------

/// `{"allow": ["Read", "Write", "FileTree"]}` — the config that produces
/// [`catbus_agent::tools::MINIMAL_TOOLS`].
///
/// Written as a literal rather than read from `MINIMAL_TOOLS` on purpose: this
/// is the file an operator would hand-write, and the point of the test is that
/// *that* JSON yields a working agent. Importing the constant would test the
/// constant against itself.
fn minimal_tools_config(dir: &Path) -> PathBuf {
    let path = dir.join("minimal-tools.json");
    std::fs::write(
        &path,
        serde_json::json!({ "allow": ["Read", "Write", "FileTree"] }).to_string(),
    )
    .unwrap();
    path
}

/// Spawn an agent with `--tools-config minimal_tools_config`, pointed at a mock
/// relay, rooted at `root`.
fn spawn_minimal_agent(
    home: &Path,
    root: &Path,
    socket: &Path,
    port: u16,
) -> (KillOnDrop, BufReader<UnixStream>, UnixStream) {
    let config = minimal_tools_config(home);
    spawn_agent_in(home, root, socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
            "--tools-config",
            config.to_str().unwrap(),
        ]);
    })
}

fn tool_names(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

/// The tool set actually offered on the wire is exactly the three, so a model
/// cannot call a capability the operator withheld.
#[test]
fn a_minimal_agent_offers_only_read_write_and_filetree() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let body = body_of(&rx.recv_timeout(Duration::from_secs(10)).unwrap());
    let mut names = tool_names(&body);
    names.sort();
    assert_eq!(
        names,
        vec!["FileTree", "Read", "Write"],
        "the offered set must be exactly the minimal trio"
    );
}

/// The task the user asked for, end to end: the agent looks around with
/// `FileTree`, reads a file with `Read`, and writes a new one with `Write` —
/// with no shell anywhere in the loop.
///
/// This is the test that would catch a `FileTree` that is registered but whose
/// output is unusable, or a `Write` that cannot be reached because nothing told
/// the model which directory it is in.
#[test]
fn a_minimal_agent_completes_a_file_task_without_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("docs")).unwrap();
    std::fs::write(work.join("docs/notes.txt"), "the catbus is late").unwrap();

    // The model's plan: see what is here, read the note, write a summary.
    //
    // Four responses for four round trips, in order — each request carries the
    // previous round's tool result, so request N+1 is where result N is found.
    // The mock blocks on accept, so a fifth would hang the test rather than
    // fail it.
    let (port, rx) = spawn_mock_relay_owned(vec![
        (
            "HTTP/1.1 200 OK",
            tool_round("c1", "FileTree", r#"{"path": ".", "depth": 2}"#),
        ),
        (
            "HTTP/1.1 200 OK",
            tool_round("c2", "Read", r#"{"path": "docs/notes.txt"}"#),
        ),
        (
            "HTTP/1.1 200 OK",
            tool_round(
                "c3",
                "Write",
                r#"{"path": "SUMMARY.md", "content": "The catbus is late."}"#,
            ),
        ),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), &work, &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "summarise the notes in docs/");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let second = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let third = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let fourth = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());

    // No shell was ever on offer — absent from the first request's specs — and
    // the task completes anyway, which is the whole claim.
    let offered = tool_names(&first);
    assert!(
        !offered.contains(&"Bash".to_string()),
        "a shell must not be reachable: {offered:?}"
    );

    // FileTree showed the real tree, so the agent could find the file.
    let tree = tool_result_of(&second, "c1");
    assert!(tree.contains("docs/"), "FileTree found no directory:\n{tree}");
    assert!(tree.contains("notes.txt"), "FileTree found no file:\n{tree}");

    // Read returned the contents, proving the path FileTree printed is the path
    // Read accepts. If the two disagreed the pair would be useless together,
    // which is why this is checked rather than assumed.
    let read = tool_result_of(&third, "c2");
    assert!(read.contains("the catbus is late"), "Read got:\n{read}");

    // Write reported success, naming the path it used, and the bytes really
    // landed there. The byte count is not asserted: its exact value depends on
    // the fixture string, and hard-coding it tests my arithmetic rather than
    // the tool — the file's contents check below is the real one.
    let write = tool_result_of(&fourth, "c3");
    assert!(
        write.starts_with("Wrote "),
        "Write should report what it wrote:\n{write}"
    );
    assert!(
        write.contains("SUMMARY.md"),
        "Write should name the file it wrote:\n{write}"
    );
    // Proves the path was resolved against the session's cwd and not the
    // process's: `work` is not the process cwd, and only the session's is.
    assert!(
        write.contains(work.to_str().unwrap()),
        "Write resolved against the wrong directory:\n{write}"
    );
    let written = std::fs::read_to_string(work.join("SUMMARY.md")).expect("SUMMARY.md should exist");
    assert_eq!(written, "The catbus is late.");
}

/// A withheld tool is refused with an error the model can act on, rather than
/// being dispatched anyway.
///
/// Worth pinning because the withholding is done by filtering `specs()`, so the
/// dispatcher has to agree with the spec list — if it did not, an agent could
/// reach Bash simply by emitting a `tool_use` for a tool it was never offered,
/// and a model that hallucinates a familiar tool name does exactly that.
#[test]
fn a_withheld_tool_cannot_be_reached_by_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    // The model asks for Bash even though it was never offered it — which is
    // what a hallucinated familiar tool name looks like on the wire.
    let (port, rx) = spawn_mock_relay_owned(vec![
        (
            "HTTP/1.1 200 OK",
            tool_round("c1", "Bash", r#"{"command": "echo pwned"}"#),
        ),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "run echo pwned");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let _first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let second = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let result = tool_result_of(&second, "c1");

    // Exact match, not a substring hunt: the loop wraps a dispatch error as
    // `Error: {e}`, so this pins both the refusal and the fact that nothing
    // else ran. `pwned` appears nowhere, because the command never executed.
    assert_eq!(
        result, "Error: unknown tool: Bash",
        "Bash must be refused by name, got:\n{result}"
    );
    assert!(!result.contains("pwned"), "the shell command ran:\n{result}");
}

/// The `/clear` socket request starts a fresh session and leaves the old
/// transcript in place.
///
/// The socket form rather than the REPL one, because this is the shape a GUI or
/// a phone uses — and "nothing is deleted" is the safety property that makes it
/// reasonable to expose at all.
#[test]
fn clearing_starts_a_fresh_session_and_keeps_the_old_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    // Have a conversation, so there is a transcript worth preserving.
    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let before = transcript_text(dir.path());
    assert!(before.contains("hi"), "the first turn should be on disk:\n{before}");

    // Clear it.
    stream.write_all(b"{\"kind\":\"clear\"}\n").unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let cleared: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(cleared["kind"], "done", "clear failed: {cleared}");
    let text = cleared["text"].as_str().unwrap();
    assert!(
        text.contains("still on disk"),
        "the reply must say the old transcript survives: {text}"
    );

    // Both transcripts now exist: the cleared one, untouched, and a new one.
    let mut files = Vec::new();
    jsonl_files(&dir.path().join(".claude").join("projects"), &mut files);
    assert_eq!(files.len(), 2, "expected the old transcript and a new one: {files:?}");
    let preserved = files
        .iter()
        .any(|f| std::fs::read_to_string(f).unwrap_or_default().contains("hi"));
    assert!(preserved, "the old transcript was destroyed by clear");
}

/// `--tools-config` with `allow` is the only way in: an agent given no config
/// keeps the full built-in set, so nothing about this change restricts a
/// normal session.
#[test]
fn a_session_without_a_tools_config_still_gets_every_tool() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    send_prompt(&mut stream, &mut reader, "hi");
    let body = body_of(&rx.recv_timeout(Duration::from_secs(10)).unwrap());
    let names = tool_names(&body);
    for expected in ["Read", "Write", "Edit", "Bash", "FileTree"] {
        assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
    }
}

// ---------------------------------------------------------------------------
// Auto mode.
//
// Three tests cover the chain, because no single one can: the REPL needs a tty
// to run at all, so the typed command cannot be driven from a subprocess test,
// and the judge needs a relay the REPL tests cannot provide. Together they pin
// that `/auto` reaches the judged mode and that the judged mode actually stops a
// write.
//
//   1. `auto_mode_consults_the_judge_and_honours_a_block` (here) — the socket's
//      `set_gate auto` makes the judge run and its verdict stop a Write.
//   2. `ansi_and_the_typed_gate_commands_reach_the_repl` (a pty test) — typing
//      `/auto` at a real prompt makes the REPL report `gate = auto`.
//   3. `the_repl_and_the_socket_agree_about_what_each_mode_is_called` (unit) —
//      `/auto` resolves through the very function the socket uses.
// ---------------------------------------------------------------------------

/// Auto mode consults the judge before a world-changing tool, and a blocking
/// verdict stops the write from happening.
///
/// The write is the point. A test that only checked for a judge *request* would
/// pass on an agent that asks the judge and then ignores the answer, which is
/// the failure mode that matters: the gate would look enabled while enforcing
/// nothing.
#[test]
fn auto_mode_consults_the_judge_and_honours_a_block() {
    let dir = tempfile::tempdir().unwrap();
    // Four responses, in order: the model asks to Write, the judge blocks it,
    // the model gives its final answer — and the mock's last entry is consumed
    // by the third request.
    let (port, rx) = spawn_mock_relay_owned(vec![
        (
            "HTTP/1.1 200 OK",
            tool_round("c1", "Write", r#"{"path": "evil.txt", "content": "x"}"#),
        ),
        // 90 is above the block threshold, so the write must not run.
        ("HTTP/1.1 200 OK", judge_verdict(90)),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), dir.path(), &socket, port);

    // Enable the gate over the socket. This is the same code path the REPL's
    // `/auto` uses — both go through `parse_gate` — so what it proves about
    // `Gate::Auto` holds for the typed command too.
    stream
        .write_all(b"{\"kind\":\"set_gate\",\"gate\":\"auto\"}\n")
        .unwrap();
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(ack["kind"], "done", "set_gate failed: {ack}");
    assert_eq!(ack["text"], "gate = auto", "unexpected ack: {ack}");

    let reply = send_prompt(&mut stream, &mut reader, "write the file");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let judge = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let third = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());

    // The judge really ran, and ran as the judge: nothing else in the loop asks
    // for `deepseek-flash`, and the judge is the only caller that passes a stop
    // sequence.
    assert_eq!(
        judge["model"], "deepseek-flash",
        "the second request should be the judge's:\n{judge}"
    );
    assert!(
        judge["stop_sequences"].is_array(),
        "the judge request should carry stop sequences:\n{judge}"
    );
    // It was asked about *our* write, not something generic: the tool and the
    // path it would touch are in the prompt.
    let judge_text = judge.to_string();
    assert!(
        judge_text.contains("evil.txt"),
        "the judge was not told what to judge:\n{judge}"
    );

    // And the block was honoured: no file, and a tool result saying so.
    assert!(
        !dir.path().join("evil.txt").exists(),
        "the write ran despite a blocking verdict"
    );
    let result = tool_result_of(&third, "c1");
    assert!(
        result.contains("blocked") || result.contains("refused"),
        "the tool result should report the refusal, got:\n{result}"
    );
    // The main loop's own request never carried the judge's model, so the two
    // kinds of call are not being conflated.
    assert_ne!(first["model"], "deepseek-flash");
}

/// A reply the server cut off at the output limit is reported as incomplete.
///
/// `stop_reason: "max_tokens"` with no tool calls lands in the same branch as a
/// clean `end_turn` — the model simply stopped, so there is no error to raise
/// and nothing to continue with. Reporting `done` over half a sentence is the
/// failure this guards; an operator reading a fragment as a whole answer is
/// worse than an error, because nothing looks wrong.
#[test]
fn a_reply_cut_off_at_the_output_limit_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let truncated = serde_json::json!({
        "id": "msg_cut",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{ "type": "text", "text": "Here is the analysis you asked for. The first thing to" }],
        "stop_reason": "max_tokens",
        "usage": { "input_tokens": 5, "output_tokens": 8192 }
    })
    .to_string();
    let (port, _rx) = spawn_mock_relay_owned(vec![("HTTP/1.1 200 OK", truncated)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "explain");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    let text = reply["text"].as_str().unwrap();

    // The fragment is still shown — it is not withheld — but flagged.
    assert!(
        text.contains("Here is the analysis"),
        "the text should survive:\n{text}"
    );
    assert!(text.contains("cut off"), "a truncated reply must be flagged:\n{text}");
    // And the flag names the limit that was hit, so the fix is discoverable.
    assert!(text.contains("8192"), "the limit should be named:\n{text}");
}

/// A clean `end_turn` carries no such warning, so the flag means something.
///
/// Without this, the test above would pass on a parser that appended the warning
/// to every reply.
#[test]
fn a_complete_reply_is_not_flagged_as_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "hi");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    let text = reply["text"].as_str().unwrap();
    assert_eq!(
        text, "hi from the relay",
        "an end_turn reply should be verbatim:\n{text}"
    );
}

/// The round-cap warning states the limit that is actually in force.
///
/// It previously hard-coded "32-round cap" while the default was 200, so an
/// operator who took it at face value would raise `CATBUS_MAX_ROUNDS` to
/// something *below* the default and observe no change at all. Set to a small
/// value here so the cap is reached in a few round trips rather than 200.
#[test]
fn the_round_cap_warning_names_the_real_limit() {
    let dir = tempfile::tempdir().unwrap();
    // Each response says a little and asks to Read again, so the loop keeps
    // going until the cap and still has prose to append the warning to. Prose
    // matters here: a loop of pure tool calls has no partial answer to annotate
    // and returns the error variant instead, which is asserted separately.
    let responses: Vec<(&'static str, String)> = (0..8)
        .map(|i| {
            (
                "HTTP/1.1 200 OK",
                talking_tool_round(
                    &format!("c{i}"),
                    "Read",
                    r#"{"path": "notes.txt"}"#,
                    &format!("reading pass {i}"),
                ),
            )
        })
        .collect();
    let (port, _rx) = spawn_mock_relay_owned(responses);
    let socket = dir.path().join("agent.sock");
    std::fs::write(dir.path().join("notes.txt"), "some notes").unwrap();

    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.env("CATBUS_MAX_ROUNDS", "3").args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "keep reading");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    let text = reply["text"].as_str().unwrap();
    assert!(
        text.contains("hit the 3-round cap"),
        "the real limit must be quoted:\n{text}"
    );
    assert!(
        !text.contains("32-round"),
        "the stale hard-coded limit is back:\n{text}"
    );
    assert!(
        text.contains("CATBUS_MAX_ROUNDS"),
        "the warning should name the knob to turn:\n{text}"
    );
}

///
/// Without this, the test above would pass on an implementation that blocked
/// every write in auto mode — which would be a different feature entirely.
#[test]
fn auto_mode_allows_a_write_the_judge_does_not_object_to() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = spawn_mock_relay_owned(vec![
        (
            "HTTP/1.1 200 OK",
            tool_round("c1", "Write", r#"{"path": "fine.txt", "content": "ok"}"#),
        ),
        ("HTTP/1.1 200 OK", judge_verdict(5)),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), dir.path(), &socket, port);

    stream
        .write_all(b"{\"kind\":\"set_gate\",\"gate\":\"auto\"}\n")
        .unwrap();
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();

    let reply = send_prompt(&mut stream, &mut reader, "write the file");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let _first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let _judge = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let third = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());

    assert_eq!(
        std::fs::read_to_string(dir.path().join("fine.txt")).expect("the write should have run"),
        "ok"
    );
    let result = tool_result_of(&third, "c1");
    assert!(
        !result.contains("blocked"),
        "a low severity should not block:\n{result}"
    );
    // The record of a check that *allowed* is the whole point: without it, a gate
    // that works leaves no trace, and "auto mode does nothing" is a report that can
    // be made about a gate doing its job. Severity 5 is the judge's answer and must
    // appear in what the operator and the next session can read.
    assert!(
        result.contains("auto checked Write"),
        "an allowed action must leave a record of the check:\n{result}"
    );
    assert!(
        result.contains("severity 5"),
        "the record should carry the judge's own number:\n{result}"
    );
    assert!(result.contains("allowed"), "and say which way it went:\n{result}");
}

/// With the gate open, no judge call is made at all.
///
/// Pins that auto mode is opt-in: if the judge ran unconditionally, every turn
/// would cost a second round trip and the `gate = open` output would be a lie.
#[test]
fn open_mode_does_not_call_the_judge() {
    let dir = tempfile::tempdir().unwrap();
    // Only two responses: a Write and then the final answer. If the judge were
    // consulted the mock would block on a third accept and the test would hang
    // rather than fail, so this also asserts the *absence* of an extra request.
    let (port, rx) = spawn_mock_relay_owned(vec![
        (
            "HTTP/1.1 200 OK",
            tool_round("c1", "Write", r#"{"path": "plain.txt", "content": "ok"}"#),
        ),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), dir.path(), &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "write the file");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let second = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    assert_ne!(
        second["model"], "deepseek-flash",
        "the judge ran in open mode:\n{second}"
    );
    assert_ne!(first["model"], "deepseek-flash");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("plain.txt")).expect("the write should have run"),
        "ok"
    );
}

/// A tool-only loop that hits the cap reports an error rather than a warning.
///
/// The other half of the same behaviour: with no prose collected there is no
/// partial answer to annotate, so the loop must not claim to have produced one.
/// Both messages have to name the knob — an operator whose agent stops has only
/// these two strings to work from.
#[test]
fn a_tool_only_loop_that_hits_the_cap_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let responses: Vec<(&'static str, String)> = (0..6)
        .map(|i| {
            (
                "HTTP/1.1 200 OK",
                tool_round(&format!("c{i}"), "Read", r#"{"path": "notes.txt"}"#),
            )
        })
        .collect();
    let (port, _rx) = spawn_mock_relay_owned(responses);
    let socket = dir.path().join("agent.sock");
    std::fs::write(dir.path().join("notes.txt"), "some notes").unwrap();

    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.env("CATBUS_MAX_ROUNDS", "3").args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "keep reading");
    assert_eq!(reply["kind"], "error", "unexpected reply: {reply}");
    let message = reply["message"].as_str().unwrap();
    assert!(message.contains("round cap"), "the cap should be named:\n{message}");
    assert!(
        message.contains("3-round"),
        "the error should quote the real limit, as the warning does:\n{message}"
    );
    assert!(
        message.contains("CATBUS_MAX_ROUNDS"),
        "the error should name the knob to turn:\n{message}"
    );
}

/// A reply that is nothing but an empty `thinking` block must not poison the
/// next request.
///
/// This is the live failure, reproduced. The model streamed a `thinking` block
/// whose text was empty while its signature survived; the turn was pushed into
/// history as-is, because the loop moves `resp.content` wholesale, and the next
/// request carried an assistant message with no content in it. Anthropic
/// answers that with `messages.N: all messages must have non-empty content` —
/// a 400 that ends the turn and, for an operator, looks like the session
/// dying for no reason.
///
/// The first reply here is that poisoned turn verbatim: the only block in the
/// message is a `thinking` block with `"thinking": ""`. The assertion is on the
/// *second* request, so it fails if the block is ever put back on the wire.
#[test]
fn a_turn_of_nothing_but_an_empty_thinking_block_is_pruned_from_the_next_request() {
    const EMPTY_THINKING: &str = r#"{
        "id": "msg_empty",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{ "type": "thinking", "thinking": "", "signature": "sig" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 5, "output_tokens": 4 }
    }"#;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("agent.sock");
    let (port, rx) = spawn_mock_relay(vec![
        ("HTTP/1.1 200 OK", EMPTY_THINKING),
        ("HTTP/1.1 200 OK", FINAL_ROUND),
        ("HTTP/1.1 200 OK", FINAL_ROUND),
    ]);
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    });

    let reply = send_prompt(&mut stream, &mut reader, "one");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let reply = send_prompt(&mut stream, &mut reader, "two");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    // The first request is the prompt on its own; the second is the one that
    // would have carried the poisoned turn.
    let _first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let raw = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let messages = body_of(&raw)["messages"].as_array().expect("messages array").clone();

    assert!(
        messages.len() >= 2,
        "the second request should carry the two prompts:\n{raw}"
    );
    assert!(
        messages.iter().all(|m| m["role"] != "assistant"),
        "the empty assistant turn should not have been sent at all:\n{raw}"
    );
    for (i, message) in messages.iter().enumerate() {
        // The env turn the harness appends is still a plain string; history is
        // in blocks. Both must be non-empty, which is the property that matters.
        if let Some(text) = message["content"].as_str() {
            assert!(!text.trim().is_empty(), "message {i} has no content:\n{raw}");
            continue;
        }
        let blocks = message["content"].as_array().expect("content is a string or blocks");
        assert!(
            !blocks.is_empty(),
            "message {i} has no content, which is the 400 this guards:\n{raw}"
        );
        for block in blocks {
            if block["type"] == "thinking" {
                assert!(
                    !block["thinking"].as_str().unwrap_or_default().trim().is_empty(),
                    "an empty thinking block reached the relay in message {i}:\n{raw}"
                );
            }
        }
    }
}

/// Opening a session that already has a transcript continues the conversation.
///
/// `session::open` defaults to the newest transcript in the cwd — the "I closed
/// the tab, I reopened it, pick up where I left off" path — and `--resume <id>`
/// names one outright. Both land in `Agent::new`, which built an empty history,
/// so the agent appended to a transcript whose contents it had never read: the
/// model got no context, and every turn it wrote was a non-sequitur on disk.
///
/// The in-REPL `/resume <id>` path rebuilt it all along; this asserts the same
/// thing is true of the entry points that start a process.
#[test]
fn resuming_a_session_gives_the_model_the_history_it_never_read() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("agent.sock");

    // One exchange, so there is a transcript with something in it.
    let (port, _rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    {
        let (_agent, mut reader, mut stream) = spawn_agent_at(dir.path(), &socket, port);
        let reply = send_prompt(&mut stream, &mut reader, "first");
        assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");
    }

    let mut files = Vec::new();
    jsonl_files(&dir.path().join(".claude").join("projects"), &mut files);
    assert_eq!(files.len(), 1, "expected one transcript: {files:?}");
    let id = files[0].file_stem().unwrap().to_str().unwrap().to_owned();

    // Resume that exact session and look at what the first request carries.
    // `spawn_agent` also passes `--new-session`; `--resume` takes precedence in
    // `session::open`, so the session opened is the one named here.
    let (port, rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);
    let (_agent, mut reader, mut stream) = spawn_agent(dir.path(), &socket, |cmd| {
        cmd.args([
            "--resume",
            &id,
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
    });
    let reply = send_prompt(&mut stream, &mut reader, "second");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let body = body_of(&rx.recv_timeout(Duration::from_secs(10)).unwrap());
    let messages = body["messages"].as_array().expect("messages");
    let text = messages.iter().map(text_of).collect::<Vec<_>>().join("\n");

    assert!(
        text.contains("first"),
        "the resumed history's prompt is missing:\n{text}"
    );
    assert!(
        text.contains("hi from the relay"),
        "the resumed reply is missing:\n{text}"
    );
    assert!(
        messages.len() >= 3,
        "the resumed turns should sit between the two prompts:\n{text}"
    );
}

/// A reply that asks for a tool must be answered, whatever its stop reason says.
///
/// `end_turn` together with a `tool_use` is contradictory, but providers do
/// emit it, and the loop trusted the stop reason over the content: it returned
/// the assistant's text and moved the turn into history with the call
/// unanswered. Every request after that carried a `tool_use` with no
/// `tool_result`, which the API rejects outright — so the session answered once
/// and then failed on every prompt until it was thrown away. The tools never
/// ran, so nothing in the reply explained why.
///
/// Whether the call ran is only observable as a second request carrying its
/// result, which is what this asserts.
#[test]
fn an_end_turn_that_asks_for_a_tool_still_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("note.txt"), "the catbus is late").unwrap();

    let contradictory = serde_json::json!({
        "id": "msg_contradiction",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "text", "text": "let me look" },
            { "type": "tool_use", "id": "c1", "name": "Read", "input": { "path": "note.txt" } }
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 5, "output_tokens": 4 }
    })
    .to_string();

    let (port, rx) = spawn_mock_relay_owned(vec![
        ("HTTP/1.1 200 OK", contradictory),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);
    let socket = dir.path().join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_minimal_agent(dir.path(), &work, &socket, port);

    let reply = send_prompt(&mut stream, &mut reader, "read the note");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    let _first = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let second = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the call was never run, so its result was never sent");
    let result = tool_result_of(&body_of(&second), "c1");
    assert!(
        result.contains("the catbus is late"),
        "the tool should have run and its output sent back: {result}"
    );
}

/// Every live process whose command line mentions `needle`.
///
/// Used to prove a sub-agent is *gone* rather than merely quiet: the socket file
/// is removed either way, so the file proves nothing, and a leaked child is
/// invisible in the reply.
fn processes_with(needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|n| n.chars().all(|c| c.is_ascii_digit())) else {
            continue;
        };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains(needle) {
            found.push(format!("{pid}: {}", cmdline.trim()));
        }
    }
    found
}

/// The socket path prefix a sub-agent is started with.
///
/// Assembled at runtime from two pieces rather than written as one literal, and
/// derived from the same `temp_dir` the tool uses. A single literal would appear
/// in the command line of whatever shell launched this test — a heredoc, a
/// `cargo test` wrapper — and the `/proc` scan below would then find *that* and
/// report a phantom leak. Same trick as the `env::args` guard in `cli`.
fn sub_agent_socket_prefix() -> String {
    std::env::temp_dir()
        .join(concat!("catbus-", "sub-"))
        .display()
        .to_string()
}

/// A sub-agent is really started, really answers, and is really reaped.
///
/// The three claims need three assertions, because each has a way of passing on
/// its own: the reply proves the child ran, the child's *own* relay receiving a
/// request proves `Spawn` started an agent rather than inventing a reply, and the
/// `/proc` scan proves it was cleaned up. A spawn path whose cleanup is untested
/// is the ordinary way orphaned agents accumulate.
///
/// The child's relay is a second mock server, reached through the environment
/// rather than a flag: a sub-agent is started with no relay flags of its own, so
/// this is also the check that a child inherits its endpoint instead of silently
/// falling back to whatever `preferences.json` holds.
#[test]
fn a_spawned_sub_agent_answers_and_is_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let marker = sub_agent_socket_prefix();

    // Before: nothing of ours is running.
    assert!(
        processes_with(&marker).is_empty(),
        "a previous run left a sub-agent behind"
    );

    // The child's relay. One canned reply, and it is what the tool result must
    // carry — a reply the parent could only have obtained by asking.
    let (child_port, child_rx) = spawn_mock_relay(vec![("HTTP/1.1 200 OK", FINAL_ROUND)]);

    // The parent's relay: a canned Spawn call, then the final answer.
    let call = tool_round(
        "s1",
        "Spawn",
        &serde_json::json!({ "task": "say hi", "cwd": work.to_str().unwrap() }).to_string(),
    );
    let (port, rx) = spawn_mock_relay_owned(vec![
        ("HTTP/1.1 200 OK", call),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ]);

    let socket = home.join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_in(home, home, &socket, |cmd| {
        // The parent's own endpoint, given as a flag.
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
        ]);
        // What a child of this agent will inherit, since a child is started with
        // no flags of its own.
        cmd.env("CATBUS_RELAY_URL", format!("http://127.0.0.1:{child_port}"));
        cmd.env("CATBUS_RELAY_TOKEN", RELAY_TOKEN);
    });

    let reply = send_prompt(&mut stream, &mut reader, "start a helper");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    // Request 1 asks for the tool; request 2 carries its result.
    let _first = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let second = rx
        .recv_timeout(Duration::from_secs(90))
        .expect("the parent never sent a second request, so Spawn never returned");
    let result = tool_result_of(&body_of(&second), "s1");

    assert!(
        result.contains("hi from the relay"),
        "the sub-agent's reply must reach the tool result: {result}"
    );
    // With `--once` the child exits by itself after answering, so the ordinary
    // footer is empty and the *only* footer that matters is the warning. Asserting
    // absence of that warning is what makes this a check on the cleanup rather
    // than on the wording; the `/proc` scan below is the real proof.
    assert!(
        !result.contains("DID NOT STOP"),
        "the sub-agent would not stop: {result}"
    );
    assert!(
        result.contains("finished in"),
        "the reply should say what the sub-agent cost in time: {result}"
    );

    // The child reached its own relay, so a real agent ran.
    child_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the sub-agent never contacted the relay it was told about");

    // And nothing is left. Retried because the kill is asynchronous: the child
    // is signalled, then reaped, and a `ps` racing that window would see a
    // dying process.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = processes_with(&marker);
        if left.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sub-agent(s) outlived the call that started them:\n  {}",
            left.join("\n  ")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The `Tasks` tool works through the dispatcher, not just in its own unit tests.
///
/// The unit tests call `apply` directly, so they would all pass with the tool
/// unregistered — never offered to the model, or offered and then rejected by the
/// dispatcher's `match`. This asserts what an agent actually experiences: the
/// schema is on the wire, `add` writes, `list` reads back what `add` wrote, and
/// the file lands under the pinned state directory.
///
/// The state directory is pinned through the environment because that is how the
/// tool resolves it, and the minimal preset deliberately does not offer `Tasks` —
/// so this builds its own tool config.
#[test]
fn the_tasks_tool_acts_through_the_dispatcher() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let work = home.join("work");
    let state = home.join("state");
    std::fs::create_dir_all(&work).unwrap();

    let config = home.join("tools.json");
    std::fs::write(&config, serde_json::json!({ "allow": ["Read", "Tasks"] }).to_string()).unwrap();

    let rounds = vec![
        (
            "HTTP/1.1 200 OK",
            tool_round(
                "t1",
                "Tasks",
                &serde_json::json!({"action":"add","title":"first"}).to_string(),
            ),
        ),
        (
            "HTTP/1.1 200 OK",
            tool_round("t2", "Tasks", &serde_json::json!({"action":"list"}).to_string()),
        ),
        ("HTTP/1.1 200 OK", FINAL_ROUND.to_owned()),
    ];
    let (port, rx) = spawn_mock_relay_owned(rounds);
    let socket = home.join("agent.sock");
    let (_agent, mut reader, mut stream) = spawn_agent_in(home, &work, &socket, |cmd| {
        cmd.args([
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            RELAY_TOKEN,
            "--tools-config",
            config.to_str().unwrap(),
        ]);
        // Pinned so a stray run cannot write into the operator's real list.
        cmd.env("XDG_STATE_HOME", &state);
    });

    let reply = send_prompt(&mut stream, &mut reader, "add a task");
    assert_eq!(reply["kind"], "done", "unexpected reply: {reply}");

    // The first request must carry the schema; otherwise nothing else matters.
    let first = body_of(&rx.recv_timeout(Duration::from_secs(30)).unwrap());
    let offered = first["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .any(|t| t["name"] == "Tasks");
    assert!(offered, "Tasks must be offered to the model:\n{first:#?}");

    let second = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let added = tool_result_of(&body_of(&second), "t1");
    assert!(added.contains("#1"), "`add` should have created task 1: {added}");
    assert!(added.contains("first"), "and echoed the title: {added}");

    let third = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let listed = tool_result_of(&body_of(&third), "t2");
    assert!(
        listed.contains("first"),
        "`list` must read back what `add` wrote: {listed}"
    );
    assert!(listed.contains("#1"), "{listed}");

    // One list, for one working directory, really on disk.
    let lists: Vec<PathBuf> = std::fs::read_dir(state.join("tab-atelier").join("agent-tasks"))
        .expect("the list directory should exist")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(lists.len(), 1, "one list for one cwd: {lists:?}");
    let raw = std::fs::read_to_string(&lists[0]).unwrap();
    assert!(raw.contains("first"), "{raw}");
}
