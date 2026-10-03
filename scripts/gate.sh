#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# gate.sh — everything CI checks, locally, in one command.
#
# fmt, clippy in both feature configurations, and the test suite. Run it before
# every push: CI runs the same steps, and a red pipeline costs a round trip.
#
#   scripts/gate.sh           # the whole gate
#   scripts/gate.sh --fast    # fmt and clippy only, no tests
#
# The test step takes minutes, so run this as a background command. `--fast` is
# quick enough to run inline.
#
# Exit: 0 all green, 1 the first failing step.
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

step 'fmt' cargo fmt --all -- --check

# The proxy crate is deliberately not a default member, so a plain
# `cargo clippy` never lints it -- it needs its own invocation. The package name
# lives in one variable rather than being spelled out at each step that targets
# it, so a rename is a one-line change here instead of a hunt.
proxy_pkg=tab-atelier-proxy
step 'clippy (default members)' cargo clippy --workspace --all-targets -- -D warnings
step 'clippy (proxy)' cargo clippy -p "$proxy_pkg" --all-targets -- -D warnings

if [ "$fast" = 0 ]; then
  # --workspace covers every member including the non-default proxy crate.
  step 'tests' cargo test --workspace
fi

if [ "$failed" = 0 ]; then
  printf '\ngate: green\n'
else
  printf '\ngate: FAILED\n' >&2
fi
exit "$failed"
