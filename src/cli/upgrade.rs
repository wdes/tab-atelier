// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

use crate::cli::share_link::{agent, discover_endpoint};

#[must_use]
pub fn run(_args: &[String]) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("upgrade: {e}");
            return 1;
        }
    };
    match agent()
        .post(format!("{}/upgrade", ep.url))
        .header("Authorization", &format!("Bearer {}", ep.token))
        .send_empty()
    {
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

#[cfg(test)]
mod tests {

    /// Through the shared client table against a real in-process daemon:
    /// the verb must arm the swap flag the owner loop polls. The test
    /// binary exists at its own path, so the re-exec target check passes.
    #[cfg(unix)]
    #[test]
    fn upgrade_arms_the_swap_through_the_daemon() {
        crate::cli::share_link::with_test_server(|_| {
            assert!(!crate::hotswap::upgrade_requested());
            assert_eq!(crate::cli::client::run("upgrade", &[]), 0);
            assert!(crate::hotswap::upgrade_requested(), "POST /upgrade arms the flag");
            crate::hotswap::clear_upgrade_request();
        });
    }
}
