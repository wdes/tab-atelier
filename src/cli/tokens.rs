// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Token-management CLI:
//!
//! - `token`              — print the master API token (so you can call
//!   the local API without hunting for the `api.token` state file).
//! - `rotate-tokens`      — revoke every tab's per-tab share tokens, so
//!   all outstanding share links 401.

use crate::cli::share_link::{agent, discover_endpoint};

/// `tab-atelier token` — print the master API token to stdout (just the
/// token, so `TOKEN=$(tab-atelier token)` works), with the base URL and
/// a usage hint on stderr.
#[must_use]
pub fn show(_args: &[String]) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("token: {e}");
            return 1;
        }
    };
    println!("{}", ep.token);
    eprintln!("# API base URL: {}", ep.url);
    eprintln!(
        "# e.g. curl -s {}/tabs -H \"Authorization: Bearer $({} token)\"",
        ep.url,
        env!("CARGO_PKG_NAME")
    );
    0
}

/// `tab-atelier rotate-tokens` — revoke every tab's per-tab share tokens.
///
/// Existing share links (GUI "Remote control" / `share-link`) immediately
/// 401; a fresh token is minted the next time you share a tab. The master
/// API token is untouched.
#[must_use]
pub fn rotate(_args: &[String]) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("rotate-tokens: {e}");
            return 1;
        }
    };
    match agent()
        .post(format!("{}/tabs/rotate-tokens", ep.url))
        .header("Authorization", &format!("Bearer {}", ep.token))
        .send_empty()
    {
        Ok(mut resp) => {
            let revoked = resp
                .body_mut()
                .read_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("revoked").and_then(serde_json::Value::as_u64))
                .unwrap_or(0);
            println!("✓ revoked share tokens on {revoked} tab(s) — old share links now 401");
            0
        }
        Err(e) => {
            eprintln!("rotate-tokens: {e}");
            1
        }
    }
}
