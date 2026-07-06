// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier remote attach <label-or-id> <tab>` — interactive
//! mirror of one remote tab into the current local terminal.
//!
//! The local terminal goes into raw mode; bytes typed on stdin are
//! forwarded to `RemoteCommand::SendInput` and bytes received via
//! `RemoteEvent::Output` are written to stdout as-is (the relay
//! already re-serialises alacritty's grid as ANSI, so SGR colour
//! escapes survive intact).
//!
//! There is no local PTY — what you see is whatever the remote is
//! drawing into its alacritty grid, refreshed at the polling
//! cadence baked into [`crate::remote`]. ~250 ms is the latency
//! floor; type-then-see is noticeably slower than a local shell
//! but workable for non-interactive operations.
//!
//! **Exit key**: Ctrl-Q (configurable later). Ctrl-C is forwarded
//! to the remote tab as a regular keystroke so the running process
//! sees it.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::cli::remote::resolver;
use crate::remote::{Client, RemoteCommand, RemoteEvent};

const READ_TICK: Duration = Duration::from_millis(50);

pub fn run(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier remote attach <label-or-id> <tab-name-or-id|#idx>");
        return 2;
    };
    let Some(tab_arg) = args.get(1) else {
        eprintln!("usage: tab-atelier remote attach <label-or-id> <tab-name-or-id|#idx>");
        return 2;
    };

    let endpoint = match resolver::endpoint(key) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tab-atelier remote attach: {e}");
            return 1;
        }
    };
    resolver::warn_if_cert_drifted(&endpoint);

    eprintln!("Connecting to {} ({})…", endpoint.label, endpoint.url);
    let Some(client) = Client::spawn(endpoint) else {
        eprintln!("tab-atelier remote attach: could not start the remote client thread");
        return 1;
    };

    let initial_tabs = match resolver::wait_for_first_tabs(&client, Duration::from_secs(5)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tab-atelier remote attach: {e}");
            return 1;
        }
    };
    let target = match resolver::pick_tab(&initial_tabs, tab_arg) {
        Ok(t) => t.clone(),
        Err(e) => {
            eprintln!("tab-atelier remote attach: {e}");
            return 1;
        }
    };
    eprintln!(
        "Attached to {} (#{}, id={}). Ctrl-Q to detach.",
        target.name,
        target.remote_index,
        &target.remote_id[..8.min(target.remote_id.len())]
    );

    if let Err(e) = run_loop(&client, &target.remote_id) {
        eprintln!("\r\ntab-atelier remote attach: {e}");
        return 1;
    }
    0
}

#[allow(clippy::match_same_arms)]
fn run_loop(client: &Client, target_id: &str) -> Result<(), String> {
    enable_raw_mode().map_err(|e| format!("enable raw mode: {e}"))?;
    let mut stdout = io::stdout();
    // Ensure raw mode is dropped no matter how we exit.
    let guard = RawModeGuard;

    let exit_flag = Arc::new(AtomicBool::new(false));
    let stdin_thread = spawn_stdin_forwarder(client.tx.clone(), target_id.to_string(), exit_flag.clone());

    let mut last_seen_len: u64 = 0;
    let mut result = Ok(());

    loop {
        if exit_flag.load(Ordering::SeqCst) || crate::SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
        match client.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(RemoteEvent::Output {
                remote_id,
                bytes,
                total_len,
                replaced,
                ..
            }) if remote_id == target_id => {
                if replaced {
                    // CRC mismatch / scrollback ring shift / alt-screen
                    // toggle: blank the local terminal so the remote's
                    // new screen renders against a clean slate. The
                    // body that follows is the full re-emitted
                    // scrollback.
                    let _ = stdout.write_all(b"\x1b[2J\x1b[H");
                }
                last_seen_len = total_len;
                if let Err(e) = stdout.write_all(&bytes) {
                    result = Err(format!("stdout: {e}"));
                    break;
                }
                let _ = stdout.flush();
            }
            Ok(RemoteEvent::Output { .. }) => {
                // Output for some other tab — ignore.
            }
            Ok(RemoteEvent::Tabs { tabs, .. }) => {
                // If the remote closed our tab, bail out cleanly.
                if !tabs.iter().any(|t| t.remote_id == target_id) {
                    eprintln!("\r\n(remote tab closed)\r");
                    break;
                }
            }
            Ok(RemoteEvent::Error { message }) => {
                eprintln!("\r\n⚠ {message}\r");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                result = Err("client thread disconnected".into());
                break;
            }
        }
    }

    exit_flag.store(true, Ordering::SeqCst);
    drop(guard); // restore terminal mode before joining
    if let Some(t) = stdin_thread {
        let _ = t.join();
    }
    eprintln!("(detached, last_len={last_seen_len})");
    result
}

fn spawn_stdin_forwarder(
    tx: std::sync::mpsc::Sender<RemoteCommand>,
    remote_id: String,
    exit_flag: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    // `None` (degraded: no keystroke forwarding, still watches) rather than
    // aborting the whole attach if the OS won't give us a thread.
    std::thread::Builder::new()
        .name("tab-atelier-attach-stdin".into())
        .spawn(move || stdin_forwarder_loop(&tx, &remote_id, &exit_flag))
        .map_err(|e| eprintln!("tab-atelier remote attach: stdin forwarder unavailable: {e}"))
        .ok()
}

#[allow(clippy::significant_drop_tightening)]
fn stdin_forwarder_loop(tx: &std::sync::mpsc::Sender<RemoteCommand>, remote_id: &str, exit_flag: &AtomicBool) {
    let mut buf = [0u8; 256];
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    loop {
        if exit_flag.load(Ordering::SeqCst) {
            break;
        }
        match handle.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                // Ctrl-Q (0x11) is the local detach key —
                // consumed locally, NOT forwarded.
                if chunk.contains(&0x11) {
                    exit_flag.store(true, Ordering::SeqCst);
                    let before: Vec<u8> = chunk.iter().take_while(|b| **b != 0x11).copied().collect();
                    if !before.is_empty() {
                        let _ = tx.send(RemoteCommand::SendInput {
                            remote_id: remote_id.to_string(),
                            bytes: before,
                        });
                    }
                    break;
                }
                if tx
                    .send(RemoteCommand::SendInput {
                        remote_id: remote_id.to_string(),
                        bytes: chunk.to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
        // Tiny yield to keep this thread from spinning when
        // stdin is non-blocking on some platforms.
        std::thread::sleep(READ_TICK);
    }
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), crossterm::style::ResetColor);
    }
}
