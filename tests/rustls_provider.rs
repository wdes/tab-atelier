// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression test for the rustls `CryptoProvider` panic.
//!
//! Workspace feature unification compiles `rustls` with both the
//! `ring` and `aws_lc_rs` features enabled (axum-server + reqwest
//! pull `aws_lc_rs` in via happier-relay and catbus-agent, while
//! tab-atelier explicitly enables `ring`). With both providers in
//! play, `rustls::ServerConfig::builder()` panics:
//!
//! ```text
//! Could not automatically determine the process-level CryptoProvider
//! from Rustls crate features.
//! ```
//!
//! The fix is to call `rustls::crypto::ring::default_provider()
//! .install_default()` in `main()` before any TLS code runs. This
//! test spawns the binary with `--check-crypto`, which exercises a
//! `ServerConfig::builder()` call and exits 0 if the provider is
//! installed. The test fails if either:
//!
//! 1. `install_default()` is accidentally removed from `main`, or
//! 2. a future workspace dep flip leaves both providers enabled with
//!    no install in place.
//!
//! NOTE: the binary spawns its own subprocess so the test runs in a
//! clean process — other `#[test]` fns in this crate can't pollute
//! the `CryptoProvider` global before we get to check it.

use std::process::Command;

#[test]
fn tab_atelier_installs_rustls_crypto_provider() {
    let bin = env!("CARGO_BIN_EXE_tab-atelier");
    let out = Command::new(bin)
        .arg("--check-crypto")
        .output()
        .expect("spawn tab-atelier --check-crypto");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Could not automatically determine the process-level CryptoProvider"),
        "rustls panicked on startup — install_default() is missing or feature graph regressed.\n\
         stderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "tab-atelier --check-crypto exited with {:?}\nstderr:\n{stderr}",
        out.status.code()
    );
}
