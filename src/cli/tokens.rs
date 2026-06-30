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

/// `tab-atelier reset-master-token` — hot-swap the master API token.
///
/// Every client / link carrying the old master token 401s on its next
/// request; the new token is persisted to `api.token` (so your saved
/// configs and `tab-atelier token` pick it up). Prints the new token to
/// stdout. Requires the current master token to authorise the request.
#[must_use]
pub fn reset_master(_args: &[String]) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("reset-master-token: {e}");
            return 1;
        }
    };
    match agent()
        .post(format!("{}/master-token/reset", ep.url))
        .header("Authorization", &format!("Bearer {}", ep.token))
        .send_empty()
    {
        Ok(mut resp) => {
            let new = resp
                .body_mut()
                .read_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("token").and_then(serde_json::Value::as_str).map(str::to_owned));
            new.map_or_else(
                || {
                    eprintln!("reset-master-token: unexpected response from daemon");
                    1
                },
                |t| {
                    println!("{t}");
                    eprintln!("# master API token reset — old token now 401s, new one written to api.token");
                    0
                },
            )
        }
        Err(e) => {
            eprintln!("reset-master-token: {e}");
            1
        }
    }
}
