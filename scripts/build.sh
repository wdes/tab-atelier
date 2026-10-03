#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# build.sh — compile everything that ships.
#
# The desktop app's default members, then the proxy, which is not a default
# member because it runs on another machine and should not be in the way of a
# desktop build.
#
#   scripts/build.sh              # debug
#   scripts/build.sh --release    # release
#
# Minutes of work: run it as a background command.
#
# Exit: 0 both built, 1 the first failure.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

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

step 'build (app)' cargo build "$@"
step 'build (proxy)' cargo build -p tab-atelier-proxy "$@"

[ "$failed" = 0 ] && printf '\nbuild: ok\n' || printf '\nbuild: FAILED\n' >&2
exit "$failed"
