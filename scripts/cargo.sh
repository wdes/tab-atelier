#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# cargo.sh — run cargo with this machine's toolchain, from anywhere.
#
# Debian's /usr/bin/cargo is 1.85 and cannot build this tree, and the rustup
# install is not on PATH in a non-login shell. Both facts, and the pinned
# version, live in lib/toolchain.sh so they are stated once; this is the way to
# get them in a shell that does not source anything.
#
#   scripts/cargo.sh build -p tab-atelier-proxy
#   scripts/cargo.sh clippy --workspace --all-targets -- -D warnings
#   scripts/cargo.sh test -p tab-atelier-proxy
#
# Redirect the output to a file rather than piping it through `tail`: a pipe
# reports the pipe's status, so a failed build reads as success.
#
# Exit: cargo's own status, or 2 for a usage error.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

if [ "$#" -eq 0 ]; then
  printf 'usage: %s <cargo args...>\n' "$0" >&2
  exit 2
fi

cargo "$@"
