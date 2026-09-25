// SPDX-License-Identifier: MPL-2.0

// Integration test crate — `.unwrap()` is idiomatic here (the crate-wide
// deny in Cargo.toml also covers `tests/`, which never sets `cfg(test)`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A turn that is *abandoned* must still tell tab-atelier it has stopped.
//!
//! The agent has two ways to run a turn: the operator types in the tab's REPL,
//! or a socket client sends one. Both can abandon a turn rather than let it
//! finish — Ctrl-C cancels the token, and a socket client that hangs up makes
//! `run_watching_for_questions` return `Ok(None)` with the turn future still
//! pinned (see the comment on that branch in `socket.rs`: "The turn is
//! abandoned rather than left running with nobody to hear its answer").
//!
//! Abandoning drops the future, so **no line after the turn's `.await` runs**.
//! The status bookkeeping used to sit exactly there, which meant an abandoned
//! turn left the status line saying `thinking` and, through `report_status`, the
//! app's indicator green — a tab that looked busy while the agent sat doing
//! nothing. The app only gave up on its own 120 s staleness sweep later.
//!
//! This drives the real hang-up path against a model that never answers, so the
//! turn can only end by being dropped, and asserts the app is told. It fails on
//! the old code by never seeing the report.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long to wait for a status report before calling it missing. Generous: a
/// report that is going to arrive arrives in milliseconds, and this only bounds
/// how long the test hangs when it is going to fail anyway.
const REPORT_DEADLINE: Duration = Duration::from_secs(10);

/// Read one HTTP/1.1 request (head, then the body `content-length` promised).
///
/// Returns what arrived, empty if the peer closed first — the caller uses that
/// as "this connection is done".
fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let promised: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= pos + 4 + promised {
                break pos + 4;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break 0,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    if head_end == 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// The JSON body of a request, decoded.
fn body_of(raw_request: &str) -> serde_json::Value {
    let body = raw_request.split_once("\r\n\r\n").map(|(_, body)| body).unwrap();
    serde_json::from_str(body).unwrap()
}

/// Stand in for tab-atelier's status route: keep every report, answer 200.
///
/// One connection at a time is enough — the report site builds a fresh client per
/// call — but it keeps reading on an open connection so a keep-alive would not be
/// mistaken for silence.
fn spawn_mock_app() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            loop {
                let raw = read_http_request(&mut stream);
                if raw.is_empty() {
                    break;
                }
                tx.send(raw).ok();
                if stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .is_err()
                {
                    break;
                }
                let _ = stream.flush();
            }
        }
    });
    (port, rx)
}

/// Stand in for the model: take the request, then never answer it.
///
/// The signal says the agent has got as far as the model, so the turn is
/// certainly in flight and pinned inside the HTTP await. Silence after that is
/// the point — the turn has to be *dropped* to end, which is what the test then
/// makes the agent do.
fn spawn_stalling_model() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            stream.set_read_timeout(Some(Duration::from_mins(1))).unwrap();
            if read_http_request(&mut stream).is_empty() {
                break;
            }
            tx.send(()).ok();
            // Hold it open and say nothing at all.
            let mut sink = [0_u8; 256];
            while let Ok(read) = stream.read(&mut sink) {
                if read == 0 {
                    break;
                }
            }
        }
    });
    (port, rx)
}

/// Stop the agent when the test ends, pass or fail. SIGTERM first — the agent
/// exits cleanly on it (socket.rs installs a handler), and a clean exit is what
/// lets a coverage-instrumented binary flush its profile to disk. SIGKILL only
/// if it has not exited within 5 s.
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

/// Spawn the agent against the stalling model, reporting status to the mock app.
///
/// `$HOME` is the tempdir (sessions never touch the real one), proxies are
/// cleared so the agent talks to the mocks directly, and `CATBUS_RELAY_URL` is
/// removed so the status report goes to `TAB_ATELIER_API_URL` — the direct path,
/// which is the one under test.
fn spawn_agent(dir: &Path, socket: &Path, model_port: u16, app_port: u16) -> KillOnDrop {
    let child = Command::new(env!("CARGO_BIN_EXE_catbus-agent"))
        .args([
            "--no-tui",
            "--new-session",
            "--cwd",
            dir.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--openai-url",
            &format!("http://127.0.0.1:{model_port}/v1"),
            "--openai-token",
            "test-token",
            "--openai-model",
            "test-model",
        ])
        .env("HOME", dir)
        .env("_TAB_ID", "tab-from-test")
        .env("TAB_ATELIER_API_URL", format!("http://127.0.0.1:{app_port}"))
        .env("TAB_ATELIER_API_TOKEN", "test-token")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("CATBUS_RELAY_URL")
        .env_remove("CATBUS_RELAY_TOKEN")
        .env_remove("CATBUS_OPENAI_URL")
        .env_remove("CATBUS_OPENAI_TOKEN")
        .env_remove("CATBUS_OPENAI_MODEL")
        .env_remove("CATBUS_PREFERENCES")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CATBUS_ANSI")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env(
            "LLVM_PROFILE_FILE",
            format!("{}/catbus-status-%p.profraw", env!("CARGO_TARGET_TMPDIR")),
        )
        .spawn()
        .unwrap();
    KillOnDrop(child)
}

/// Poll until the agent's socket accepts, then complete the handshake.
fn connect_socket(path: &Path) -> (BufReader<UnixStream>, UnixStream) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        if let Ok(stream) = UnixStream::connect(path) {
            break stream;
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

/// Wait for the next report whose `state` is `want`, out of `rx`.
///
/// Reports that are not the one waited for are skipped rather than failing the
/// test, so an earlier one (the agent may say `waiting` as it starts up) cannot
/// make this depend on report ordering it does not mean to test.
fn wait_for_state(rx: &mpsc::Receiver<String>, want: &str) -> serde_json::Value {
    let deadline = Instant::now() + REPORT_DEADLINE;
    loop {
        let left = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
        let raw = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no `{want}` report within {REPORT_DEADLINE:?}"));
        let body = body_of(&raw);
        if body["state"] == want {
            return body;
        }
    }
}

#[test]
fn abandoned_turn_reports_that_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (app_port, reports) = spawn_mock_app();
    let (model_port, model_hit) = spawn_stalling_model();
    let socket = dir.path().join("agent.sock");
    let _agent = spawn_agent(dir.path(), &socket, model_port, app_port);

    let (reader, mut stream) = connect_socket(&socket);
    let request = serde_json::json!({ "kind": "prompt", "text": "say something" });
    stream.write_all(format!("{request}\n").as_bytes()).unwrap();

    // The turn is now certainly in flight: the model has the request and will
    // never answer it.
    model_hit
        .recv_timeout(Duration::from_secs(30))
        .expect("the agent never reached the model");
    let thinking = wait_for_state(&reports, "thinking");
    assert_eq!(thinking["agentKind"], "catbus");

    // Hang up mid-turn, exactly as a client that closes its terminal does. The
    // agent treats this as "nobody is waiting for the answer" and drops the turn.
    drop(stream);
    drop(reader);

    // The regression: the app must hear that the turn is over. On the old code
    // the report sites sat after the turn's `.await`, which a dropped future
    // never reaches, so this times out and the tab keeps showing it as working.
    let stopped = wait_for_state(&reports, "waiting");
    assert_eq!(stopped["agentKind"], "catbus");
    assert!(
        stopped["sessionId"].as_str().is_some_and(|id| !id.is_empty()),
        "the report must carry the session the tab resumes with: {stopped}"
    );
}
