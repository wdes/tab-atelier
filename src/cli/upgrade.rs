// SPDX-License-Identifier: MPL-2.0

//! `upgrade` — hot-swap the running instance, tabs stay live.
//!
//! Asks the running GUI or headless daemon to re-exec the binary
//! currently installed at its own path, keeping every tab's shell
//! alive across the switch.
//!
//! Typical flow: `apt upgrade tab-atelier` (or copying a new binary
//! over the installed one), then `tab-atelier upgrade`. The running
//! process re-`exec()`s the new file, handing each tab's live PTY over
//! — nothing running inside the tabs is restarted. See `src/hotswap.rs`
//! for the mechanism.

use crate::cli::client::{authed_post, discover_endpoint};

/// `tab-atelier upgrade` — arm a hot swap on the running daemon.
#[must_use]
pub fn run(_args: &[String]) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("upgrade: {e}");
            return 1;
        }
    };
    match authed_post(&ep, "/upgrade").send_empty() {
        Ok(mut resp) => {
            let pid = resp
                .body_mut()
                .read_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("pid").and_then(serde_json::Value::as_u64));
            match pid {
                Some(pid) => println!(
                    "✓ hot swap armed (pid {pid}) — the process re-execs the installed binary \
                     within a couple of seconds; every tab stays live"
                ),
                None => println!("✓ hot swap armed — every tab stays live"),
            }
            0
        }
        Err(e) => {
            eprintln!("upgrade: {e}");
            1
        }
    }
}
