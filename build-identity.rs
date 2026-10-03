// SPDX-License-Identifier: MPL-2.0

// The build identity every binary in this workspace reports.
//
// Included by each crate's `build.rs` rather than copied into it, because what
// counts as "this build" has to be one answer: two binaries compiled from the
// same commit must not disagree about which commit that was, and the only thing
// the string is for is telling whether two builds are the same. A second copy of
// this logic is a second chance to drift.
//
// Written as `//` comments rather than `//!` because `include!` splices this in
// below the includer's own documentation, where an inner doc comment is not
// allowed.
//
// Identity is picked in this order:
//   1. `git rev-parse --short=12 HEAD` — 12-char hex, e.g. `07c49210abcd`
//   2. UNIX timestamp at compile time, formatted as `t<secs>`,
//      e.g. `t1717590000`. Used when no `.git/` is present (source
//      tarball builds).
//   3. The literal `"unknown"`, only if even `SystemTime::now()` fails
//      (genuinely bizarre clock state).
//
// The timestamp fallback rather than a constant string is deliberate: a tarball
// user who unpacks a new release sees fresh mtimes, so cargo re-runs the build
// script and the identity changes. A constant would report `unknown` for every
// tarball build, and anything comparing two builds would stop seeing upgrades.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Emit the identity to cargo, and watch the file that changes when HEAD moves.
///
/// `git_head_log` is the path to `.git/logs/HEAD` **relative to the package that
/// includes this file**, which is why it is a parameter rather than a constant: a
/// crate is not always at the repository root, and cargo resolves the path
/// against the package root. A wrong path is silent — cargo ignores one that does
/// not exist — so the hash would freeze at whatever the first build saw, which is
/// the one failure this directive exists to prevent.
fn emit_build_hash(git_head_log: &str) {
    // 12 hex characters is enough entropy to disambiguate any two builds in this
    // repository's lifetime and short enough to read at a glance in logs.
    let identity = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // The `t` prefix distinguishes a timestamp from a git hash at a
            // glance, so logs are never ambiguous about which they are reading.
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| format!("t{}", d.as_secs()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_HASH={identity}");

    // Re-run when HEAD moves: a commit on the current branch, or a checkout to
    // another one. `.git/logs/HEAD` is the cheapest watcher that catches both —
    // every commit and every branch switch appends a line — where watching
    // `.git/HEAD` alone would miss ordinary commits, because that file just holds
    // `ref: refs/heads/main` and does not change.
    //
    // Harmless on the no-git path: the file does not exist, so it never fires and
    // the identity stays pinned to the unpack time, which is what a tarball wants.
    println!("cargo:rerun-if-changed={git_head_log}");
    println!("cargo:rerun-if-env-changed=BUILD_HASH");
}
