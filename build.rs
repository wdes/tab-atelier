// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Captures the git commit hash at build time and exposes it as
//! `BUILD_HASH` via `env!`. The headless API embeds it into the
//! share-link viewer HTML + the `X-Build-Hash` response header so
//! the viewer can show an "↻ update available" chip when the binary
//! it's been served by changes — without false-positives on plain
//! daemon restarts (which would have flipped a random boot id).
//!
//! Falls back to `"unknown"` when git isn't available or when we're
//! building from a source tarball outside a repo. The viewer treats
//! `"unknown"` the same way it treats empty — comparison is skipped.

use std::process::Command;

fn main() {
    // `git rev-parse --short=12 HEAD` is enough entropy to disambiguate
    // any two distinct builds in this repo's lifetime and short enough
    // to read at a glance in logs / Vary headers.
    let hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_HASH={hash}");

    // Re-run the script when HEAD moves (commits on the current
    // branch OR a checkout to a different branch). `.git/logs/HEAD`
    // is the cheapest watcher that catches both: every commit and
    // every branch switch appends a line. Watching `.git/HEAD`
    // alone misses regular commits because the file just contains
    // `ref: refs/heads/main`, which doesn't change.
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    // Also rerun if cargo invokes us with a different `git` on PATH,
    // or if these env vars change (e.g. CI override).
    println!("cargo:rerun-if-env-changed=BUILD_HASH");
}
