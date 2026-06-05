// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Captures a build-identity string at compile time and exposes it
//! as `BUILD_HASH` via `env!`. The headless API embeds it into the
//! share-link viewer HTML + the `X-Build-Hash` response header so
//! the viewer can show an "↻ update available" chip when the binary
//! it's been served by changes — without false-positives on plain
//! daemon restarts.
//!
//! Identity is picked in this order:
//!   1. `git rev-parse --short=12 HEAD` — 12-char hex, e.g. `07c49210abcd`
//!   2. UNIX timestamp at compile time, formatted as `t<secs>`,
//!      e.g. `t1717590000`. Used when no `.git/` is present (source
//!      tarball builds).
//!   3. The literal `"unknown"`, only if even `SystemTime::now()` fails
//!      (genuinely bizarre clock state). Viewer short-circuits the
//!      comparison so we don't false-positive into a chip.
//!
//! Why a timestamp fallback rather than a constant string: a tarball
//! user who unpacks a new release will see fresh mtimes → cargo
//! re-runs build.rs → new timestamp → viewer detects the upgrade.
//! Without it, every tarball build would identify as `unknown` and
//! the chip would never fire.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // `git rev-parse --short=12 HEAD` is enough entropy to
    // disambiguate any two distinct builds in this repo's lifetime
    // and short enough to read at a glance in logs / Vary headers.
    let identity = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // Tarball fallback. The `t` prefix distinguishes
            // timestamps from git hashes at a glance so logs aren't
            // ambiguous.
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| format!("t{}", d.as_secs()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_HASH={identity}");

    // Re-run the script when HEAD moves (commits on the current
    // branch OR a checkout to a different branch). `.git/logs/HEAD`
    // is the cheapest watcher that catches both: every commit and
    // every branch switch appends a line. Watching `.git/HEAD`
    // alone misses regular commits because the file just contains
    // `ref: refs/heads/main`, which doesn't change.
    //
    // For the tarball/no-git path, this directive is harmless (the
    // file doesn't exist, so it never fires) — the timestamp stays
    // pinned at unpack time, which is exactly what we want: a
    // rebuild of the same tarball produces the same identity, but a
    // fresh tarball unpack refreshes mtimes and the timestamp.
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    println!("cargo:rerun-if-env-changed=BUILD_HASH");
}
