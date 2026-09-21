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

    /// Drain output until the rendered screen satisfies `pred`, or time out.
    ///
    /// Needed in place of [`Pty::drain_until`] for the question panel, because ratatui diffs its
    /// buffer: a repaint writes only the cells that *changed*, so a value that appears on screen
    /// never shows up in the stream as contiguous text. Typing into the note is the clearest case
    /// — each keystroke changes one cell, so the typed word is spread across many paints with
    /// escape sequences between the letters. Emulating the screen is the only view of what the
    /// operator actually sees.
    fn drain_until_screen(&self, seen: &mut String, rows: u16, pred: impl Fn(&str) -> bool, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if pred(&screen_of(seen, rows, 80).join("\n")) {
                return true;
            }
            if Instant::now() >= deadline {
                // One last look, in case the final paint landed as the deadline passed.
                return pred(&screen_of(seen, rows, 80).join("\n"));
            }
            // A needle nothing can match: this only pumps the channel for a moment, so the loop
            // re-renders on every 100ms of output rather than spinning.
            let _ = self.drain_until(seen, "\u{0}no such needle\u{0}", Duration::from_millis(100));
        }
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
    // Waits for `error:` and not `error`: the needle is what the harness blocks on, and the word
    // alone matches other lines — a log entry about a failed request, say — so the wait would
    // return before the REPL had said anything, and the assertion below would then be reading
    // output that was still arriving. The colon is what makes this the REPL's own line.
    let (_, seen) = type_and_expect("/automatic", "error:");
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
/// Read one HTTP request, returning it as text.
///
/// The text is what lets a caller tell the app's startup price fetch (`GET`) from a turn
/// (`POST`) — both arrive on the same port, and answering only one of them is the difference
/// between a test that exercises a turn and one that answers the wrong request.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let Ok(n) = stream.read(&mut chunk) else {
            return String::new();
        };
        if n == 0 {
            return String::new();
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
        let Ok(n) = stream.read(&mut chunk) else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Serve one reply, after `delay`, and return the port.
///
/// The delay is the point: it is what gives the spinner time to paint frames, so
/// a test can observe the in-flight line rather than only the final totals.
fn spawn_delayed_relay(body: &'static str, delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // One *turn* is enough — a successful reply ends it — but the app makes two requests: a
        // price fetch at startup, then the turn. So the loop accepts until the canned body has
        // been served, and answers the price fetch without consuming it. Skipping this is how the
        // GET ended up eating the only reply and the turn never finished.
        let mut served = false;
        while !served {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let raw = read_request(&mut stream);
            if raw.starts_with("GET ") {
                let prices = MOCK_PRICES;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{prices}",
                    prices.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                continue;
            }
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            served = true;
        }
    });
    port
}

/// The price list the mock serves for `GET /v1/models`.
///
/// Two models in two currencies, so the totals a session accumulates can be shown to group rather
/// than to add: a session that used one reply from each has spent dollars and euros, and no single
/// figure expresses that. The ids match the `model` the canned replies report, so a reply's model
/// is one this catalog prices.
const MOCK_PRICES: &str = r#"{
    "models": [
        { "id": "claude-sonnet-4-6", "name": "Claude Sonnet (mock)",
          "amounts": [
            { "currency": "USD", "unit_tokens": 1000000, "kind": "input", "price": 3.0 },
            { "currency": "USD", "unit_tokens": 1000000, "kind": "output", "price": 15.0 },
            { "currency": "USD", "unit_tokens": 1000000, "kind": "cache_read", "price": 0.3 }
          ] },
        { "id": "deepseek-flash", "name": "DeepSeek flash (mock)",
          "amounts": [
            { "currency": "EUR", "unit_tokens": 1000000, "kind": "input", "price": 2.0 },
            { "currency": "EUR", "unit_tokens": 1000000, "kind": "output", "price": 10.0 }
          ] }
    ]
}"#;

/// The transcript as the test sees it, with colour escapes removed.
///
/// Needed for two reasons. The spinner and the totals line both carry SGR, and a
/// literal search for `12,345 in` would otherwise have to interleave escape codes the
/// test does not care about. And ratatui repaints a line by returning to its start, so
/// a capture holds the old frame and the new one with a carriage return between them —
/// which turns `the bytes we sent` into `the lines a reader sees`, and is what a table
/// needs before its columns can be compared.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        // A paint that overwrites begins with CR; on screen nothing of the old frame
        // remains, so it is not part of the text.
        if ch == '\r' {
            continue;
        }
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

/// A canned reply whose text is markdown with a table in it.
///
/// The width is not arbitrary: the pty is 80 columns, and the instruction tells the
/// model to keep a table narrow enough to read in 80 — so this is the shape the
/// requirement is about, not a convenient one.
const REPLY_WITH_TABLE: &str = r###"{
    "id": "msg_table",
    "type": "message",
    "role": "assistant",
    "model": "claude-sonnet-4-6",
    "content": [{ "type": "text", "text": "## Results\n\n| Name | Count | Note |\n|------|-------|------|\n| alpha | 1 | first |\n| beta | 22 | second, longer |\n| gamma | 333 | third |\n\nDone." }],
    "stop_reason": "end_turn",
    "usage": { "input_tokens": 10, "output_tokens": 10 }
}"###;

/// The screen a terminal would be showing, given what the app wrote to it.
///
/// This is the point of the test below being worth anything. A pty capture is not a picture of a
/// terminal: ratatui writes only the cells that changed since its last frame, and repaints by
/// moving an absolute cursor, so the raw bytes contain no spaces that were already spaces — a
/// typed `make me a table` arrives as `makemeatable` — and a repaint puts the old and new frame on
/// one line separated by a carriage return. Asserting on that measures the encoder, not the
/// screen, which is why the earlier version of this test could not check alignment at all and
/// said so in its own doc comment.
///
/// Feeding the bytes through a real VT parser instead gives the thing a person would be looking
/// at: cells at positions, after every move and erase has been applied. A utility complained
/// about the weak version of this test; this is the answer to it.
///
/// `rows` is deliberately larger than the pty's. The app draws an inline viewport from the size it
/// is told, and everything above it is pushed into scrollback with `insert_before` — so on an
/// emulator of exactly 24 rows the table would scroll out of the visible area and the assertion
/// would be reading blank space. Emulating a taller screen keeps the whole session present without
/// changing what the app renders.
fn screen_of(captured: &str, rows: u16, cols: u16) -> Vec<String> {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(captured.as_bytes());
    let screen = parser.screen();
    // `size` rather than `rows`/`cols`: `rows` counts scrollback as well, and there is none here.
    let (emulated_rows, emulated_cols) = screen.size();
    (0..emulated_rows)
        .map(|row| {
            let line: String = (0..emulated_cols)
                .map(|col| {
                    // An untouched cell reports `""`, but a blank terminal cell *is* a space — and
                    // the difference matters here. ratatui writes only the cells that changed, and
                    // an unstyled space is indistinguishable from a blank cell, so it is skipped.
                    // Mapping `""` to `""` would therefore erase every space the app did not style,
                    // and the failure would look like the app dropping text when it had rendered
                    // it perfectly.
                    screen.cell(row, col).map_or_else(
                        || " ".to_owned(),
                        |cell| {
                            let contents = cell.contents();
                            if contents.is_empty() {
                                " ".to_owned()
                            } else {
                                contents.to_owned()
                            }
                        },
                    )
                })
                .collect();
            line.trim_end().to_owned()
        })
        .collect()
}

/// A table the model wrote must be **seen** as an aligned table, and paste as markdown.
///
/// Two properties that pull against each other, which is why they are asserted together:
/// box-drawing characters would align beautifully and paste as mojibake, and raw markdown pastes
/// perfectly and reads as a wall of pipes. Padding the cells and keeping the pipes is meant to
/// satisfy both, and only a screen can show that it does.
#[test]
fn a_markdown_table_is_aligned_on_screen_and_pastes_back_as_markdown() {
    let port = spawn_delayed_relay(REPLY_WITH_TABLE, Duration::from_millis(0));
    let (ok, seen) = type_and_expect_at(port, &[], "make me a table", "Done.");
    assert!(ok, "the reply should have reached the screen:\n{seen}");

    // The screen, after every cursor move and erase has been applied.
    let screen = screen_of(&seen, 200, 80);
    let rows: Vec<&str> = screen
        .iter()
        .map(String::as_str)
        .filter(|line| line.trim_start().starts_with('|'))
        .collect();
    assert!(
        rows.len() >= 4,
        "the header, the rule and the data rows should be on screen; the whole screen was:\n{}",
        screen.join("\n")
    );

    // --- aligned on screen -------------------------------------------------
    // Every pipe lands in the same column in every row. Measured in characters, since the padding
    // is a character count: a byte index would report a misalignment that is not on the screen.
    let columns = |row: &str| -> Vec<usize> {
        row.chars()
            .enumerate()
            .filter(|(_, c)| *c == '|')
            .map(|(i, _)| i)
            .collect()
    };
    let want = columns(rows[0]);
    assert_eq!(want.len(), 4, "three columns means four pipes: {:?}", rows[0]);
    for row in &rows {
        assert_eq!(
            columns(row),
            want,
            "columns must line up across every row:\n{}\n{}",
            rows.join("\n"),
            cells_shown(row)
        );
    }

    // --- and pastes back as markdown ---------------------------------------
    assert!(
        rows.iter().any(|r| r.contains("---")),
        "the rule row survives, which is what makes the selection markdown:\n{}",
        rows.join("\n")
    );
    for row in &rows {
        assert_eq!(row.matches('|').count(), 4, "each row is still a table row: {row}");
    }
    // Box-drawing would satisfy the alignment above and break this: it renders as a table and
    // pastes as characters nobody can reuse.
    for glyph in [
        '\u{250c}', '\u{2500}', '\u{252c}', '\u{2510}', '\u{2502}', '\u{2514}', '\u{2534}', '\u{2518}',
    ] {
        assert!(
            !rows.iter().any(|r| r.contains(glyph)),
            "box-drawing {glyph:?} would not survive a paste:\n{}",
            rows.join("\n")
        );
    }

    // --- the values and the prose around them ------------------------------
    let screen_text = screen.join("\n");
    for value in [
        "Name",
        "Count",
        "Note",
        "alpha",
        "beta",
        "gamma",
        "333",
        "second, longer",
    ] {
        assert!(
            screen_text.contains(value),
            "{value:?} is missing from the screen:\n{screen_text}"
        );
    }
    assert!(
        screen_text.contains("Results"),
        "the heading text is shown:\n{screen_text}"
    );
    assert!(
        !screen_text.contains("##"),
        "the heading's marks were consumed by the renderer:\n{screen_text}"
    );
    assert!(
        screen_text.contains("Done."),
        "the text after the table is printed:\n{screen_text}"
    );
}

/// Show a row's cells with their padding made visible, for a failure message.
///
/// A failing alignment assertion is otherwise almost impossible to read: the two rows look
/// identical in a terminal dump and differ by one space somewhere in the middle.
fn cells_shown(row: &str) -> String {
    format!("{row:?}")
}

/// The colour flags still change what the *renderer* does, which is all they do now.
///
/// `--ansi` used to pick a prompt variant and decide whether a reply was filtered;
/// both are gone. What is left is whether the REPL styles its rendering, and that is
/// invisible to a client reading the socket — so the visible difference is only in a
/// terminal, which is what this checks.
#[test]
fn no_colour_prints_the_markdown_unrendered() {
    let port = spawn_delayed_relay(REPLY_WITH_TABLE, Duration::from_millis(0));
    let (ok, seen) = type_and_expect_at(port, &[("NO_COLOR", "1")], "make me a table", "Done.");
    assert!(ok, "the reply should have reached the screen:\n{seen}");
    let plain = strip_ansi(&seen);
    // The text is all there — nothing is lost to the colour opt-out — but the marks
    // are shown rather than rendered, because that is what "no colour, no styling"
    // means. The same characters reach the socket either way.
    assert!(
        plain.contains("## Results"),
        "with no colour the markdown is shown as written:\n{plain}"
    );
    assert!(plain.contains("Done."), "{plain}");
}

/// A reply that asks a question instead of answering, as the tool-use path would.
const REPLY_ASKING: &str = r#"{
    "id": "msg_ask",
    "type": "message",
    "role": "assistant",
    "model": "claude-sonnet-4-6",
    "content": [{
        "type": "tool_use",
        "id": "toolu_ask",
        "name": "AskUserQuestion",
        "input": {
            "questions": [{
                "question": "Which one should it be?",
                "header": "pick",
                "multiSelect": false,
                "options": [
                    { "label": "first", "description": "the first one" },
                    { "label": "second", "description": "the second one" }
                ]
            }]
        }
    }],
    "stop_reason": "tool_use",
    "usage": { "input_tokens": 100, "output_tokens": 20 }
}"#;

/// A relay that asks a question, then keeps the request that carries the answer.
///
/// Two posts: the first gets [`REPLY_ASKING`], so the app draws the panel and blocks on the
/// answer; the second can only exist once the question has been answered, because it carries the
/// tool result. That second request is sent down the channel because it is the only place the
/// operator's keystrokes become visible to a test — the answer is a tool result inside the request
/// body, and nothing on screen shows it. The turn is then ended with a plain reply so the screen
/// settles and the test can prove the turn resumed rather than just stopping.
fn spawn_asking_relay() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut posts = 0;
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let raw = read_request(&mut stream);
            // The startup price fetch shares the port but is not a turn.
            let body = if raw.starts_with("GET ") {
                MOCK_PRICES
            } else {
                posts += 1;
                if posts == 1 { REPLY_ASKING } else { REPLY_WITH_USAGE }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            if posts == 2 {
                let _ = tx.send(raw);
                return;
            }
        }
    });
    (port, rx)
}

/// A REPL on a pty with the **full** tool set, handed back so keys can be sent mid-turn.
///
/// [`type_and_expect_at`] cannot do this job: it sends one line and returns, and this test has to
/// stay in the middle of a turn to press keys while the agent is blocked on the answer. The tool
/// set is left at its default because `AskUserQuestion` is not in `MINIMAL_TOOLS`.
///
/// The temp home is returned rather than dropped, because the child's config and socket live in it
/// — a `TempDir` that goes out of scope takes the directory out from under a live process.
fn repl_against(port: u16) -> (Pty, KillOnDrop, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    // Not `mut`: `send` takes `&mut self`, and the caller owns it by then.
    let pty = Pty::open();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catbus-agent"));
    cmd.env("HOME", &home)
        // Quiet, and not just for tidiness. The agent logs with `tracing` to stdout, and this test
        // asserts on the rendered screen — but the log writer does not know ratatui owns the
        // screen, so an INFO line lands wherever the cursor happens to be. Mid-viewport that
        // overwrites the prompt, and ratatui does not repaint it, because its own buffer still
        // holds the text it thinks is there. Whether the log arrives before or after a paint is
        // timing, so at `info` the screen assertions are flaky rather than wrong.
        //
        // The underlying defect is real and is not this test's to fix — the agent should route
        // its logs through `Ui::print_above`, or to a file, once the TUI owns the terminal.
        .env("RUST_LOG", "error")
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
            "--relay-url",
            &format!("http://127.0.0.1:{port}"),
            "--relay-token",
            "tap_pty_test",
        ]);
    let child = cmd
        .stdin(pty.stdio())
        .stdout(pty.stdio())
        .stderr(pty.stdio())
        .spawn()
        .expect("spawn catbus-agent");
    (pty, KillOnDrop(child), dir)
}
/// The question panel is driven by tick-box keystrokes, and what they put on the wire is the
/// chosen *label* and the note — a seam no unit test reaches, because the panel and the socket
/// sit on opposite sides of a task boundary. A pty is the only place both are real.
///
/// The keys are the terminal bytes a keyboard would send, so this pins the decoding too: a
/// panel that understood `j`/`k` but not the arrow keys would pass every unit test and fail here.
#[test]
fn the_question_panel_sends_the_ticked_label_and_the_note() {
    let (port, answered) = spawn_asking_relay();
    let (mut pty, _child, _home) = repl_against(port);

    let mut seen = String::new();
    assert!(
        pty.drain_until(&mut seen, "/help", Duration::from_secs(20)),
        "the REPL never drew its prompt:\n{seen}"
    );
    pty.send("ask me something\n");
    assert!(
        pty.drain_until(&mut seen, "Which one should it be?", Duration::from_secs(30)),
        "the question never appeared:\n{seen}"
    );
    // The prompt text and the panel come from the same loop iteration but are flushed as
    // separate writes, so waiting on the question alone races the paint. Everything below reads
    // the *screen*, because the panel is drawn in the viewport and a repaint emits only the cells
    // that changed — the raw stream is not a picture of anything.
    assert!(
        pty.drain_until_screen(
            &mut seen,
            60,
            |screen| screen.contains("[ ] first") && screen.contains("[ ] second"),
            Duration::from_secs(30)
        ),
        "the panel never painted its options:\n{}",
        screen_of(&seen, 60, 80).join("\n")
    );

    // `ESC [ B` is the down arrow: move to the second option, then tick it with space.
    pty.send("\u{1b}[B");
    pty.send(" ");
    assert!(
        pty.drain_until_screen(
            &mut seen,
            60,
            |screen| screen.contains("[x] second"),
            Duration::from_secs(15)
        ),
        "space must tick the option the cursor moved to:\n{}",
        screen_of(&seen, 60, 80).join("\n")
    );
    // The cursor mark has to sit on the ticked option, or the operator cannot tell which box
    // space will hit — and a tick on the wrong option is a silently wrong answer.
    assert!(
        screen_of(&seen, 60, 80).join("\n").contains("▸[x] second"),
        "the cursor must be on the option that was ticked:\n{}",
        screen_of(&seen, 60, 80).join("\n")
    );

    // `n` opens the note, which covers the hint row — so `note:` is the only new text that
    // proves the field is open and taking keys.
    pty.send("n");
    assert!(
        pty.drain_until_screen(
            &mut seen,
            60,
            |screen| screen.contains("note:"),
            Duration::from_secs(15)
        ),
        "`n` must open the note field:\n{}",
        screen_of(&seen, 60, 80).join("\n")
    );

    // Typed with no terminator, because Enter sends. Each keystroke changes one cell, so the
    // word is only ever whole on the reconstructed screen.
    pty.send("only-if-applied");
    assert!(
        pty.drain_until_screen(
            &mut seen,
            60,
            |screen| screen.contains("only-if-applied"),
            Duration::from_secs(15)
        ),
        "what is typed must appear in the note:\n{}",
        screen_of(&seen, 60, 80).join("\n")
    );

    // Enter sends: the note is the last thing written, so requiring a second Enter here would be
    // a dead end with nothing on screen to signpost it.
    pty.send("\r");

    let raw = answered
        .recv_timeout(Duration::from_secs(20))
        .expect("the answer must produce a second request");
    // The answer is the tool *result*, a JSON string inside the request body, so its quotes are
    // escaped — unlike the `"label": …` of the tool *input*, which is a nested object. That is
    // what makes `\"second\"` a statement about the answer rather than about the question.
    assert!(raw.contains(r#"\"chosen\""#), "the reply must carry the choices: {raw}");
    assert!(
        raw.contains(r#"\"second\""#),
        "the ticked label must reach the wire: {raw}"
    );
    assert!(
        !raw.contains(r#"\"first\""#),
        "the option that was not ticked must not be sent: {raw}"
    );
    assert!(
        raw.contains("only-if-applied"),
        "the note must reach the wire beside the choice: {raw}"
    );
    assert!(
        raw.contains("Which one should it be?"),
        "and the answer must name the question it answers: {raw}"
    );

    // The turn resumed rather than stopping at the answer.
    assert!(
        pty.drain_until(&mut seen, "Hello there.", Duration::from_secs(20)),
        "the turn should continue once the question is answered:\n{seen}"
    );
}

/// A relay that answers every turn plainly, and hands over the first request body.
///
/// The body is the only place a submitted prompt is visible to a test: the request is what the
/// agent sends upstream, so a newline that survived the editor and the socket is a literal `\n`
/// inside the JSON `text` field.
fn spawn_capturing_relay() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut captured = false;
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let raw = read_request(&mut stream);
            // The startup price fetch shares the port but is not a turn.
            let is_turn = !raw.starts_with("GET ");
            let body = if is_turn { REPLY_WITH_USAGE } else { MOCK_PRICES };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            if is_turn && !captured {
                captured = true;
                let _ = tx.send(raw);
            }
        }
    });
    (port, rx)
}

/// Shift+Enter puts a newline in the prompt, and the newline reaches the API.
///
/// The bytes sent are the literal kitty-protocol sequence, which is the whole point of the test:
/// it is what tab-atelier's `keystroke_to_bytes` emits once catbus has pushed
/// `DISAMBIGUATE_ESCAPE_CODES`, and what crossterm decodes back into `Enter` + SHIFT. Encoding it by
/// hand here covers the contract both sides have to agree on — a change to either that broke the
/// agreement would fail here and nowhere else.
///
/// Two things are checked, because they can fail independently: that the prompt *displays* as two
/// rows (the viewport grew and wrapped), and that the newline survives into the request.
#[test]
fn shift_enter_puts_a_newline_in_the_prompt() {
    let (port, captured) = spawn_capturing_relay();
    let (mut pty, _child, _home) = repl_against(port);

    let mut seen = String::new();
    assert!(
        pty.drain_until(&mut seen, "/help", Duration::from_secs(20)),
        "the REPL never drew its prompt:\n{seen}"
    );
    pty.send("first line");
    // Shift+Enter, as the kitty protocol encodes it: ESC [ 13 ; 2 u.
    pty.send("\u{1b}[13;2u");
    pty.send("second line");

    // The two lines have to be drawn on separate rows, and this has to be checked *before*
    // submitting: Enter clears the prompt out of the viewport, so afterwards there is nothing on
    // the live screen to check. A screen is the only place the newline is visible at all, since
    // ratatui repaints only the cells that changed and the prompt therefore never appears
    // contiguously in the raw byte stream.
    // 60 rows, not the pty's 24: `screen_of` documents that the emulated screen has to be
    // *taller* than the terminal, or what the app pushes above its inline viewport scrolls out of
    // the visible area and the assertion reads blank space.
    assert!(
        pty.drain_until_screen(
            &mut seen,
            60,
            |screen| screen.contains("first line") && screen.contains("second line"),
            Duration::from_secs(15)
        ),
        "both lines must be on screen:\n{}",
        screen_of(&seen, 60, 80)
            .iter()
            .enumerate()
            .map(|(i, row)| format!("{i:>2}|{row}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let screen = screen_of(&seen, 60, 80).join("\n");
    let rows: Vec<&str> = screen.lines().collect();
    let first = rows.iter().position(|row| row.contains("first line"));
    let second = rows.iter().position(|row| row.contains("second line"));
    assert!(
        matches!((first, second), (Some(a), Some(b)) if b > a),
        "the two lines must be on separate rows, in order:\n{screen}"
    );

    // Then submit, and check what the buffer actually held. The request is the ground truth: it
    // carries the prompt as JSON, so a newline that survived the editor and the socket is a
    // literal `\n` inside the `text` field — the bytes on the wire being a backslash and an `n`,
    // which is what a raw string literal matches. Their being *contiguous* is also the proof that
    // the prompt stayed one message: two messages could not produce a single text value holding
    // both lines.
    pty.send("\r");
    let raw = captured
        .recv_timeout(Duration::from_secs(20))
        .expect("the prompt must reach the relay");
    assert!(
        raw.contains(r"first line\nsecond line"),
        "the newline must survive into the request:\n{raw}"
    );
}
