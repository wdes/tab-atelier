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
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut pty = Pty::open();

    let child = Command::new(env!("CARGO_BIN_EXE_catbus-agent"))
        // Hermetic, for the same reasons as the socket tests: no operator config,
        // no inherited colour settings.
        .env("HOME", home)
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
            // A refused-connection port. The relay is only contacted when a
            // prompt is sent, and this test never sends one, so the URL only has
            // to be well-formed.
            "--relay-url",
            "http://127.0.0.1:9",
            "--relay-token",
            "tap_pty_test",
        ])
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
