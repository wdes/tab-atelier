// SPDX-License-Identifier: MPL-2.0

// Integration test crate — unwrap/expect are idiomatic here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The REPL, driven through a real pseudo-terminal.
//!
//! One test lives here because nothing else can reach it. `run_repl` uses
//! reedline, which opens `/dev/tty` itself rather than reading stdin — so piping
//! into the process fails with ENXIO and the whole slash-command path stays
//! invisible to a normal subprocess test.
//!
//! That path is where `/auto` lives, and the failure it can hide is specific: a
//! command that prints `gate = auto` while selecting something else is
//! indistinguishable from a working one from the outside. The socket tests prove
//! `Gate::Auto` consults the judge and a unit test proves `/auto` parses to
//! `Gate::Auto`; this closes the last link, that reedline delivers the typed line
//! to the handler at all.
//!
//! `nix::pty::openpty` rather than libc's: the project denies `unsafe_code`.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A pty pair, with the master split into a writer and a reading thread.
///
/// Split because the reading side has to run on its own thread: a blocking read
/// on a pty master does not return once the child stops producing output, so the
/// timeout is enforced by the *consumer* instead of the read.
struct Pty {
    writer: File,
    slave: OwnedFd,
    chunks: mpsc::Receiver<Vec<u8>>,
}

impl Pty {
    fn open() -> Self {
        // A sane window size: reedline asks the terminal for its dimensions, and
        // a 0x0 pty makes it render into nothing, so the prompt would be
        // unreadable to the assertions below.
        let winsize = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pair = nix::pty::openpty(Some(&winsize), None).expect("openpty");
        let master = pair.master;
        let slave = pair.slave;

        let writer = File::from(master.try_clone().expect("dup master"));
        let responder = File::from(master.try_clone().expect("dup master for DSR"));
        let mut reader = File::from(master);
        let (tx, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            // A real terminal answers the cursor-position query, and reedline
            // asks it by writing `ESC [ 6 n` then blocking on the reply. A
            // harness that only reads leaves that request unanswered, and
            // reedline gives up with "The cursor position could not be read
            // within a normal duration" — so the reply is not optional
            // politeness, it is what makes the REPL usable at all.
            //
            // Handled on this thread rather than by the test, so a query is
            // answered whenever it arrives: reedline emits one at startup, long
            // before the test is waiting for anything in particular.
            let mut responder = responder;
            // Queries can straddle a read boundary, so the search runs over the
            // last few bytes of the previous chunk plus this one.
            let mut carry: Vec<u8> = Vec::new();
            let mut buf = [0_u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut window = Vec::with_capacity(carry.len() + n);
                window.extend_from_slice(&carry);
                window.extend_from_slice(&buf[..n]);
                let queries = window.windows(4).filter(|w| *w == b"\x1b[6n").count();
                for _ in 0..queries {
                    // Row 1, column 1: any position is accepted, it is only
                    // used to place the cursor for redraws.
                    let _ = responder.write_all(b"\x1b[1;1R");
                }
                let _ = responder.flush();
                carry = window[window.len().saturating_sub(4)..].to_vec();

                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self { writer, slave, chunks }
    }

    /// A fresh handle on the slave, for one of the child's standard streams.
    /// Each stream needs its own, since `Stdio` takes ownership.
    fn stdio(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().expect("dup slave"))
    }

    fn send(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).expect("write to pty");
        self.writer.flush().expect("flush");
    }

    /// Accumulate output into `seen` until it contains `needle`, or time out.
    /// Returns whether the needle arrived.
    ///
    /// Takes `&self`: reading is a shared operation, and the receiving half of
    /// the channel is `Sync` — only `send` needs the writer, which is why `send`
    /// takes `&mut self` and this does not.
    fn drain_until(&self, seen: &mut String, needle: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.chunks.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    // Lossy: reedline emits escape sequences and the odd split
                    // multi-byte character. The needles searched for are plain
                    // ASCII, so a mangled neighbour cannot hide one.
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    if seen.contains(needle) {
                        return true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The child closed the pty, so nothing more will arrive.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        seen.contains(needle)
    }
}

/// Stop the child on the way out, so a failed assertion cannot leave a process
/// holding a pty open.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Type `line` at a live REPL and report whether it answered with `expect`.
///
/// Each command is sent once the prompt has been seen, so the child is certainly
/// reading before the next line arrives. Sending them immediately also works —
/// the pty buffers — but would make a timing failure much harder to read.
fn type_and_expect(line: &str, expect: &str) -> (bool, String) {
    type_and_expect_env(&[], line, expect)
}

/// As [`type_and_expect`], with extra environment variables set on the child.
///
/// The env is the input to the colour decision, so testing that decision means
/// controlling it explicitly — and `RUST_LOG` is needed because the verdict is
/// logged at `info`, which REPL mode suppresses unless it is asked for.
fn type_and_expect_env(env: &[(&str, &str)], line: &str, expect: &str) -> (bool, String) {
    // Port 9 (discard) is deliberately dead: these tests never send a prompt, so
    // the relay is never contacted and the URL only has to be well-formed.
    type_and_expect_at(9, env, line, expect)
}

/// As [`type_and_expect_env`], pointed at a live relay on `port`.
///
/// Split out so a test that *does* send a prompt can aim at a mock relay. The
/// dead-port default above is what keeps the other tests from needing one.
fn type_and_expect_at(port: u16, env: &[(&str, &str)], line: &str, expect: &str) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut pty = Pty::open();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catbus-agent"));
    cmd
        // Hermetic, for the same reasons as the socket tests: no operator config,
        // no inherited colour settings.
        .env("HOME", home)
        .env("RUST_LOG", "info")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CATBUS_ANSI")
        .env_remove("CATBUS_TOOLS_CONFIG")
        .env_remove("CATBUS_PREFERENCES")
        .args([
            "--new-session",
            "--cwd",
            home.to_str().unwrap(),
            "--socket",
            home.join("agent.sock").to_str().unwrap(),
            // Minimal tools. Nothing here sends a prompt, so the tool set is
            // irrelevant to the assertion — but it keeps the agent from reading
            // the operator's own repository while the test runs.
            "--tools-config",
            "minimal",
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            "tap_pty_test",
        ]);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let child = cmd
        // All three streams on the pty: reedline writes the prompt to stdout and
        // reads keys from the controlling terminal, so anything left as a pipe
        // would either hide the output or fail the open.
        .stdin(pty.stdio())
        .stdout(pty.stdio())
        .stderr(pty.stdio())
        .spawn()
        .expect("spawn catbus-agent");
    let _child = KillOnDrop(child);

    let mut seen = String::new();
    // The banner and the first prompt are what say the REPL is live, so the
    // command is not typed into a process that is still resolving its relay.
    let ready = pty.drain_until(&mut seen, "/help", Duration::from_secs(20));
    assert!(ready, "the REPL never drew its prompt; output so far:\n{seen}");

    pty.send(&format!("{line}\n"));
    let matched = pty.drain_until(&mut seen, expect, Duration::from_secs(20));
    (matched, seen)
}

/// The colour verdict the binary reaches from its environment.
///
/// Stands in for the tab case that was broken: a real terminal (`stdout_renders`
/// true, since this is a pty) whose environment says no colour. Reading `TERM`
/// only would decide `true` and emit escapes into a tab the user had explicitly
/// switched to monochrome, so both signals are checked here.
#[test]
fn a_colours_off_tab_gets_a_plain_agent() {
    // `TERM=dumb` is what the app's per-tab toggle set; `NO_COLOR` is the
    // standard signal it now sets alongside. Either must be enough on its own,
    // so both are exercised — a fix that only read one would still be broken for
    // whichever path sends the other.
    for env in [
        &[("TERM", "dumb")][..],
        &[("TERM", "dumb"), ("NO_COLOR", "1")][..],
        &[("NO_COLOR", "1")][..],
    ] {
        let (_, seen) = type_and_expect_env(env, "/noplan", "gate = open");
        assert!(
            seen.contains("ansi escapes in replies: false"),
            "env {env:?} should disable escapes; output was:\n{seen}"
        );
    }
}

/// And a colour-capable terminal keeps them, so the fix is not a blanket off.
#[test]
fn a_colour_capable_tab_keeps_escapes() {
    let (_, seen) = type_and_expect_env(&[("TERM", "xterm-256color")], "/noplan", "gate = open");
    assert!(
        seen.contains("ansi escapes in replies: true"),
        "an ordinary terminal should keep escapes; output was:\n{seen}"
    );
}

#[test]
fn typing_auto_at_the_repl_selects_auto_mode() {
    // The end of the chain the user asked about: `/auto` typed at a prompt makes
    // the REPL report `gate = auto`. The socket test proves that gate consults
    // the judge and honours a block; the unit test proves `/auto` parses to it.
    // This is the link in between, and the only one needing a terminal.
    let (matched, seen) = type_and_expect("/auto", "gate = auto");
    assert!(matched, "`/auto` did not report auto mode; output was:\n{seen}");
}

#[test]
fn the_other_mode_commands_report_their_own_gate() {
    // `/auto` alone would pass on a handler that printed "gate = auto" for any
    // input, so the neighbouring commands are checked against the same harness —
    // and `/plan` especially, since a handler that mangled the argument would
    // most plausibly confuse two adjacent words.
    for (command, expected) in [("/plan", "gate = plan"), ("/noplan", "gate = open")] {
        let (matched, seen) = type_and_expect(command, expected);
        assert!(matched, "`{command}` did not report {expected}; output was:\n{seen}");
    }
}

#[test]
fn a_typo_is_treated_as_a_prompt_not_a_command() {
    // `/automatic` must not switch modes. It is sent as a prompt, so the agent
    // tries to reach its relay and fails — which is the *proof* it was treated as
    // a prompt rather than as a command. Asserting on the connection error rather
    // than on a mode name is deliberate: it distinguishes "reached the model
    // path" from "silently ran a slash command".
    let (_, seen) = type_and_expect("/automatic", "error");
    assert!(
        seen.contains("error:"),
        "a near-miss command should have been sent as a prompt; output was:\n{seen}"
    );
    // And no gate was reported at all.
    assert!(
        !seen.contains("gate = "),
        "a typo changed the gate; output was:\n{seen}"
    );
}

// ---------------------------------------------------------------------------
// The status line: the spinner during a download, and the totals line after.
//
// These need a *reply*, so unlike the tests above they point the agent at a mock
// relay rather than a dead port. The formatting is unit-tested in
// `statusline.rs` without a terminal; what can only be checked here is that the
// REPL feeds it the right values and actually paints them — a totals line wired
// to the wrong field would pass every unit test in that module.
// ---------------------------------------------------------------------------

/// A reply with non-trivial `usage`, so the totals line has real numbers to
/// show and the expected string is known exactly.
const REPLY_WITH_USAGE: &str = r#"{
    "id": "msg_pty",
    "type": "message",
    "role": "assistant",
    "model": "claude-sonnet-4-6",
    "content": [{ "type": "text", "text": "Hello there." }],
    "stop_reason": "end_turn",
    "usage": { "input_tokens": 12345, "output_tokens": 6789 }
}"#;

/// Read one HTTP/1.1 request (headers plus a `Content-Length` body).
///
/// The body has to be drained, not just the headers: the request carries the
/// whole conversation and tool specs, and answering before it is fully read can
/// reset the connection instead of replying.
fn read_request(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let Ok(n) = stream.read(&mut chunk) else { return };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let length: usize = String::from_utf8_lossy(&buf[..header_end])
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let Ok(n) = stream.read(&mut chunk) else { return };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Serve one reply, after `delay`, and return the port.
///
/// The delay is the point: it is what gives the spinner time to paint frames, so
/// a test can observe the in-flight line rather than only the final totals.
fn spawn_delayed_relay(body: &'static str, delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // One request is enough: a successful reply ends the turn, so the client
        // makes exactly one.
        if let Ok((mut stream, _)) = listener.accept() {
            read_request(&mut stream);
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

/// The transcript as the test sees it, with colour escapes removed.
///
/// Needed because the spinner and the totals line both carry SGR, and a literal
/// search for `12,345 in` would otherwise have to interleave escape codes that
/// the test does not care about.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // `ESC [` then parameters and a final letter: the only kind emitted here.
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn a_turn_paints_the_spinner_and_then_the_totals_line() {
    // A colour-capable tab, because the spinner line carries SGR and the
    // `\r\x1b[K` repaint is only meaningful on a terminal that interprets it.
    let port = spawn_delayed_relay(REPLY_WITH_USAGE, Duration::from_secs(3));
    let (matched, seen) = type_and_expect_at(port, &[("TERM", "xterm-256color")], "hello", "12,345 in");

    assert!(
        matched,
        "the totals line never appeared; output was:\n{}",
        strip_ansi(&seen)
    );
    let flat = strip_ansi(&seen);

    // The totals line, with the server's own numbers and the mode alongside.
    assert!(flat.contains("12,345 in - 6,789 out"), "totals line malformed:\n{flat}");
    assert!(flat.contains("manual"), "the mode field is missing:\n{flat}");
    // Its whole purpose is to sit under the reply, so the reply must precede it.
    let reply_at = flat.find("Hello there.").expect("the reply should be shown");
    let totals_at = flat.find("12,345 in - 6,789 out").unwrap();
    assert!(reply_at < totals_at, "the totals line should follow the reply:\n{flat}");

    // The in-flight spinner painted at least one frame, with a token count.
    assert!(
        flat.contains("tokens in"),
        "no spinner frame carried a token count:\n{flat}"
    );
    assert!(
        flat.contains("Thinking"),
        "the spinner never showed the activity label:\n{flat}"
    );
    // And the count is marked as the local estimate, not the server's figure —
    // the distinction the `~` exists to make.
    assert!(
        flat.contains('~'),
        "the in-flight count must be marked as an estimate:\n{flat}"
    );
}

/// Whether the text contains a CSI cursor-position sequence: `ESC [ <r> ; <c> H`.
///
/// Looked for structurally rather than as a literal, because the sequence that
/// immediately precedes a paint is often an SGR colour (`ESC [ 38;5;8;49 m`) and
/// taking the *last* escape finds that instead of the move. Both are `ESC [`, and
/// only the final byte distinguishes them.
fn has_cursor_move(text: &str) -> bool {
    text.match_indices('\u{1b}').any(|(i, _)| {
        let rest = &text[i + 1..];
        let Some(params) = rest.strip_prefix('[') else {
            return false;
        };
        let mut chars = params.chars();
        let mut digits = 0;
        // Parameter bytes: digits and separators, at least one.
        while matches!(chars.clone().next(), Some(c) if c.is_ascii_digit() || c == ';') {
            chars.next();
            digits += 1;
        }
        // Then the final byte. `H` is "cursor position".
        digits > 0 && chars.next() == Some('H')
    })
}

#[test]
fn the_spinner_repaints_rather_than_appending() {
    // The property is that the status row is *repainted*, so a long activity label
    // cannot accumulate: each frame replaces the last. The previous renderer did that
    // by hand — back to column 0, erase to end of line — and this test asserted those
    // exact bytes (`\r\x1b[K`). ratatui repaints by diffing its buffer against the
    // previous one and writing only the cells that changed, so the erase sequence is
    // simply absent and the byte assertion stopped describing anything.
    //
    // Within this capture there is only one frame to look at, for a reason worth
    // recording: the expectation stops at `tokens in`, and that text is painted on the
    // *first* status frame. So the two things that can be checked here are that the
    // animation started, and that the frame was painted at a cursor position rather
    // than written as a new line — which is exactly the difference between repainting
    // and appending, and is what the old assertion was reaching for.
    let port = spawn_delayed_relay(REPLY_WITH_USAGE, Duration::from_secs(2));
    let (_, seen) = type_and_expect_at(port, &[], "hello", "tokens in");

    assert!(
        "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().any(|c| seen.contains(c)),
        "a spinner frame should be on screen: {seen:?}"
    );

    let at = seen.find("Thinking").expect("the activity label should be on screen");
    let before = &seen[at.saturating_sub(40)..at];
    assert!(
        has_cursor_move(before),
        "the status row must be painted at a cursor position, not appended as a line; \
         bytes before the label: {before:?}"
    );
    assert!(
        !before.ends_with('\n') && !before.ends_with("\r\n"),
        "the label must not be written on a line of its own: {before:?}"
    );
}
