// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! REAL busy-tab round-trip for aligator's submit-confirm fix (feat/aligator-
//! submit-confirm). Anti built≠wired: it drives an ISOLATED headless daemon
//! (the outbox recipe) with a tab running a fake-TUI stand-in that reproduces
//! the exact bug — a trailing `\r` arriving before the paste has settled is
//! ABSORBED (Enter lost, text stuck in the input box), while a `\r` after the
//! box settles SUBMITS.
//!
//!  * RED-before: the OLD path (type → fixed 400 ms → `\r`) fires the Enter mid-
//!    ingest → the stand-in absorbs it → the text stays stuck in the box.
//!  * GREEN-after: the real `aligator --once` tick (settle-poll + submit-confirm
//!    + bounded busy→idle retry) drains the swamped text once the box settles.
//!
//! No mock of the drain: every assertion reads the tab's REAL `/output` screen.
//!
//! Gated to the `headless` (no-`catbus`) build: that build ships the
//! `tab-atelier-headless` bin this test launches AND omits the catbus session
//! sweep that would otherwise wipe the stamped `agent_kind=claude`. Run with:
//!   cargo test -p tab-atelier --no-default-features --features headless,energy
#![cfg(all(feature = "headless", not(feature = "catbus")))]
// Integration test crate — `.unwrap()`/`.expect()` is idiomatic here (a failed
// one is how the test reports the unexpected), the same opt-in the other tests use.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// std::env::set_var is `unsafe` in edition 2024; safe here (one test / binary,
// no concurrent env access), so opt out of the crate-wide unsafe_code gate.
#![allow(unsafe_code)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tab_atelier::cli::aligator;

/// A fake Claude-ish input box that models paste-ingest lag: an Enter landing
/// within `SETTLE_S` of the last typed character is ABSORBED (the bug — the char
/// isn't echoed, the text stays on the input line); an Enter after the box has
/// settled submits and clears it. The `[SUBMITTED N chars]` line is printed
/// WITHOUT the text, so a drained screen no longer carries the needle.
const STANDIN: &str = r#"
import os, sys, termios, tty, time
SETTLE_S = 1.0
fd = sys.stdin.fileno()
def w(x):
    sys.stdout.write(x); sys.stdout.flush()
old = termios.tcgetattr(fd)
tty.setraw(fd)
buf = ""
last_text = 0.0
w("STANDIN-READY\r\n> ")
try:
    while True:
        chunk = os.read(fd, 1)
        if not chunk:
            break
        c = chunk.decode("latin1")
        if c == "\x03":
            break
        if c in ("\r", "\n"):
            if time.time() - last_text < SETTLE_S:
                pass  # busy: Enter absorbed, char not echoed -> text stuck on the line
            else:
                w("\r\n[SUBMITTED %d chars]\r\n> " % len(buf))
                buf = ""
        else:
            buf += c
            last_text = time.time()
            w(c)  # echo onto the input line
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"#;

/// Kills the daemon on drop (before the `TempDir` it lives under is removed).
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into()
}

fn poll<F: FnMut() -> bool>(what: &str, secs: u64, mut f: F) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

fn send_input(ag: &ureq::Agent, base: &str, auth: &str, id: &str, bytes: &[u8]) {
    ag.post(format!("{base}/tabs/by-id/{id}/input"))
        .header("Authorization", auth)
        .header("Content-Type", "application/octet-stream")
        .send(bytes)
        .expect("POST /input");
}

fn read_output(ag: &ureq::Agent, base: &str, auth: &str, id: &str) -> String {
    ag.get(format!("{base}/tabs/by-id/{id}/output"))
        .header("Authorization", auth)
        .call()
        .expect("GET /output")
        .body_mut()
        .read_to_string()
        .expect("read /output")
}

fn tabs(ag: &ureq::Agent, base: &str, auth: &str) -> Vec<serde_json::Value> {
    ag.get(format!("{base}/tabs"))
        .header("Authorization", auth)
        .call()
        .expect("GET /tabs")
        .body_mut()
        .read_json::<serde_json::Value>()
        .expect("parse /tabs")
        .get("tabs")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn busy_tab_submit_is_stuck_before_and_drains_after() {
    if Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |s| !s.success())
    {
        eprintln!("SKIP: python3 unavailable — the fake-TUI stand-in needs it");
        return;
    }

    // Isolated daemon: a free port + a throwaway HOME (own token, tabs, state).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config/tab-atelier")).unwrap();
    std::fs::write(
        home.join(".config/tab-atelier/preferences.json"),
        format!("{{\"api_addr\":\"127.0.0.1:{port}\"}}"),
    )
    .unwrap();
    let standin_path = tmp.path().join("standin.py");
    std::fs::write(&standin_path, STANDIN).unwrap();

    let Some(bin) = option_env!("CARGO_BIN_EXE_tab-atelier-headless") else {
        eprintln!("SKIP: tab-atelier-headless not built this invocation");
        return;
    };
    let child = Command::new(bin)
        .env("HOME", &home)
        // Live /output (no snapshot re-scan throttle) so the settle-poll + confirm
        // read the real grid deterministically; the perf throttle is prod-only.
        .env("KALPIN_SNAPSHOT_THROTTLE_MS", "0")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("TAB_ATELIER_API_URL")
        .env_remove("TAB_ATELIER_API_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tab-atelier-headless");
    // Declared AFTER `tmp` → dropped BEFORE it: kill the daemon, then rm the dir.
    let _daemon = Daemon(child);

    let base = format!("http://127.0.0.1:{port}");
    let token_path = home.join(".local/state/tab-atelier/api.token");
    let mut token = String::new();
    poll("api.token", 15, || {
        token = std::fs::read_to_string(&token_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        !token.is_empty()
    });
    let auth = format!("Bearer {token}");
    let ag = agent();

    // A live tab (the seed) → run the stand-in in it.
    poll("a tab to exist", 15, || !tabs(&ag, &base, &auth).is_empty());
    let tab_id = tabs(&ag, &base, &auth)[0]
        .get("id")
        .and_then(|v| v.as_str())
        .expect("tab id")
        .to_string();

    // Wait for the shell prompt, launch the stand-in, wait for its banner.
    poll("shell prompt", 10, || {
        !read_output(&ag, &base, &auth, &tab_id).trim().is_empty()
    });
    send_input(
        &ag,
        &base,
        &auth,
        &tab_id,
        format!("python3 {}\r", standin_path.display()).as_bytes(),
    );
    poll("stand-in ready", 15, || {
        read_output(&ag, &base, &auth, &tab_id).contains("STANDIN-READY")
    });

    // Stamp the tab a live Claude agent so aligator's confused-deputy gate lets
    // the swamp deliver to it.
    ag.post(format!("{base}/tabs/by-id/{tab_id}/status"))
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .send(r#"{"state":"thinking","agentKind":"claude","sessionId":"test-sess"}"#)
        .expect("POST /status");
    poll("agent_kind=claude", 10, || {
        tabs(&ag, &base, &auth)
            .iter()
            .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(tab_id.as_str()))
            .and_then(|t| t.get("agent_kind").and_then(|v| v.as_str()))
            == Some("claude")
    });

    // ---- RED: the OLD recipe (type → fixed 400 ms → ⏎) fires mid-ingest ------
    let red = "RED-STUCK-PLEASE-RUN-TESTS";
    send_input(&ag, &base, &auth, &tab_id, red.as_bytes());
    std::thread::sleep(Duration::from_millis(400)); // the old fixed SUBMIT_DELAY
    send_input(&ag, &base, &auth, &tab_id, b"\r");
    std::thread::sleep(Duration::from_millis(300));
    let stuck = read_output(&ag, &base, &auth, &tab_id);
    assert!(
        !aligator::input_drained(red, &stuck),
        "RED: a fixed-400ms ⏎ during ingest is absorbed → text stuck in the box:\n{stuck}"
    );
    assert!(!stuck.contains("[SUBMITTED"), "RED: nothing was submitted:\n{stuck}");

    // Sanity: a settled ⏎ drains the stuck text, so the box is clean for GREEN.
    std::thread::sleep(Duration::from_millis(1200));
    send_input(&ag, &base, &auth, &tab_id, b"\r");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        aligator::input_drained(red, &read_output(&ag, &base, &auth, &tab_id)),
        "sanity: a settled ⏎ drains the box"
    );

    // ---- GREEN: the real `aligator --once` drain settle-polls + confirms -----
    let state_dir = tmp.path().join("aligstate");
    std::fs::create_dir_all(&state_dir).unwrap();
    // SAFETY: the only test in this binary → no concurrent env access.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", &state_dir);
        std::env::set_var("TAB_ATELIER_API_URL", &base);
        std::env::set_var("TAB_ATELIER_API_TOKEN", &token);
    }
    let green = "GREEN-DRAIN-DELIVER-NOW";
    let entry = aligator::SwampEntry {
        ts: 0,
        tab: tab_id.clone(),
        input: green.to_string(),
        submit: true,
        from: None,
        attempts: 0,
        priority: aligator::Priority::default(),
        dedup_key: None,
    };
    aligator::append_swamp_line(&aligator::swamp_path(), &entry).unwrap();

    // The real tick: GET /tabs → deliver → settle-poll → ⏎ → confirm drained.
    assert_eq!(aligator::run(&["--once".to_string()]), 0, "aligator --once exits clean");

    std::thread::sleep(Duration::from_millis(300));
    let after = read_output(&ag, &base, &auth, &tab_id);
    assert!(
        after.contains("[SUBMITTED"),
        "GREEN: the swamped text was SUBMITTED once the box settled:\n{after}"
    );
    assert!(
        aligator::input_drained(green, &after),
        "GREEN: the swamped text drained from the input box:\n{after}"
    );
}
