#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# gate.sh — everything CI checks, run locally, in CI's order.
#
#   scripts/gate.sh              # the full gate
#   scripts/gate.sh --fast       # skip the headless matrix (the slow half)
#
# `scripts/build.sh` compiles what ships; this is the *verification* gate, and it
# exists because a narrower loop has now cost this repo two red CI runs:
#
#   * the default `cargo test` skips the proxy, because it is not a default member;
#   * `cargo test` cannot see the Kiosk's browser JS at all — those tests are plain
#     `.mjs` files run by `crates/tab-atelier-kiosk/assets/run-tests.sh`, which CI
#     runs as its own step.
#
# Both were green locally and red in CI. The fix is not "remember harder": it is to
# have one command whose failure means what CI's failure means.
#
# Minutes of work: run it as a background command.
#
# Exit: 0 everything passed, 1 the first failing step (and the rest still run).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

fast=0
[ "${1:-}" = "--fast" ] && fast=1

failed=0
step() {
  local label="$1"
  shift
  printf '\n=== %s\n' "$label"
  if "$@"; then
    printf '%s\n' "--- ok: $label"
  else
    printf '%s\n' "--- FAILED: $label"
    failed=1
  fi
}

# Formatting first: it is the cheapest and the most common trivial failure.
step 'fmt' cargo fmt --all -- --check

# The no-energy build: CI does it first, and the `energy` feature is off in a
# default desktop build, so a `cfg(feature = "energy")` mistake only shows here.
step 'build (no energy)' cargo build --no-default-features

step 'clippy (workspace)' cargo clippy --workspace --all-targets -- -D warnings

# `--workspace` already covers the proxy, which is deliberately not a default
# member. The explicit lint stays as a belt-and-braces: if someone ever narrows the
# workspace argument above, this is the step that keeps the proxy from silently
# dropping out of the gate — and the proxy is the crate that gets touched least
# often and reviewed least closely.
proxy_pkg=tab-atelier-proxy
step 'clippy (proxy)' cargo clippy -p "$proxy_pkg" --all-targets -- -D warnings

# `--workspace` and not the default members: the proxy and the kiosk are the two
# crates a bare invocation misses, and they are the two most likely to be touched.
step 'test (workspace)' cargo test --workspace

# The Kiosk's browser JS. `cargo test` cannot see it; CI runs it as its own step.
step 'kiosk assets' crates/tab-atelier-kiosk/assets/run-tests.sh

if [ "$fast" = 0 ]; then
  step 'build (headless)' cargo build --no-default-features --features headless
  step 'test (headless)' cargo test --no-default-features --features headless
  # Clippy on the headless feature set separately: a `#[cfg(feature = "gui")]` arm
  # that does not compile is invisible to the GUI lint and to a default test run.
  step 'clippy (headless)' cargo clippy --no-default-features --features headless --all-targets -- -D warnings
fi

if [ "$failed" = 0 ]; then
  printf '\ngate: ok\n'
else
  printf '\ngate: FAILED — fix before pushing\n' >&2
fi
exit "$failed"
