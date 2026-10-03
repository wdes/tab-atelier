// SPDX-License-Identifier: MPL-2.0

//! Captures a build-identity string at compile time and exposes it
//! as `BUILD_HASH` via `env!`. The headless API embeds it into the
//! share-link viewer HTML + the `X-Build-Hash` response header so
//! the viewer can show an "↻ update available" chip when the binary
//! it's been served by changes — without false-positives on plain
//! daemon restarts.
//!
//! The identity itself, and why it is picked the way it is, lives in
//! `build-identity.rs` at the repository root — shared with every other binary
//! in the workspace so two builds of one commit cannot report different hashes.

include!("build-identity.rs");

fn main() {
    // `.git/logs/HEAD` from the repository root, which is also this package's
    // root: cargo resolves the path against the package, not the build script.
    emit_build_hash(".git/logs/HEAD");
}
