// SPDX-License-Identifier: MPL-2.0

// Integration test crate — unwrap/expect are idiomatic here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `--version` names the build, not just the crate version.
//!
//! A `catbus-agent` running in a tab can be older than the checkout that started it — the binary
//! the app runs is not rebuilt by every `cargo build` — and this string is how anyone finds that
//! out. The app answers in the same shape, `v0.5.0-dev (2ee7a0329f23)`, so two binaries built from
//! one commit read alike.
//!
//! Checked by running the binary rather than by reading `CARGO_PKG_VERSION` in a unit test: the
//! identity arrives through the build script's environment, and only a real run shows whether it
//! landed. The freshness check at the end is the one that matters most — it fails if the build
//! script stops noticing that HEAD moved, which is a hash quietly reporting the wrong commit rather
//! than an obviously missing one.

use std::process::Command;

/// The build identity this checkout is at, when git can be asked.
fn git_head() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn version_names_the_build_it_came_from() {
    let out = Command::new(env!("CARGO_BIN_EXE_catbus-agent"))
        .arg("--version")
        .output()
        .expect("run catbus-agent --version");
    assert!(out.status.success(), "--version failed: {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();

    // `catbus-agent v0.1.0-dev (<identity>)`
    let rest = text
        .strip_prefix("catbus-agent ")
        .unwrap_or_else(|| panic!("the line should name the binary: {text:?}"));
    let (version, identity) = rest
        .split_once(" (")
        .unwrap_or_else(|| panic!("the version should carry a build identity: {text:?}"));
    let identity = identity
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("the identity should be bracketed: {text:?}"));
    assert!(!identity.is_empty(), "the identity is empty: {text:?}");
    assert_eq!(
        version,
        format!("v{}", env!("CARGO_PKG_VERSION")),
        "the crate version should be what it says, prefixed with `v`: {text:?}"
    );

    // One of the three shapes the build script can produce: a 12-character commit hash, a
    // `t<seconds>` stamp for a build with no git to ask, or `unknown` when even the clock could not
    // be read. Anything else means a second identity was invented somewhere.
    let is_commit = identity.len() == 12 && identity.chars().all(|c| c.is_ascii_hexdigit());
    let is_stamp = identity
        .strip_prefix('t')
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
    assert!(
        is_commit || is_stamp || identity == "unknown",
        "the identity is neither a commit hash, a timestamp, nor `unknown`: {identity:?}"
    );

    // And when there is a checkout to compare against, the identity must be *this* commit. This is
    // what catches the build script losing its watch on `.git/logs/HEAD`: the hash would still look
    // well-formed, and would be reporting a commit the tree has since moved past.
    if let Some(head) = git_head() {
        assert_eq!(
            identity, head,
            "the binary reports a different commit than the checkout: {text:?}"
        );
    }
}
