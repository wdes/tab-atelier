#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# package-deb.sh — build the Debian package.
#
# Always `-p tab-atelier`. The workspace also holds catbus-agent, and a .deb of
# that on its own is not a thing we ship: it is a component of the agent, not a
# product, and a standalone package of it invites installing a half of a pair.
#
#   scripts/package-deb.sh
#
# Minutes of work: run it as a background command.
#
# Exit: 0 the .deb was written under target/debian/, 1 it was not.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

printf '=== cargo deb -p tab-atelier\n'
if cargo deb -p tab-atelier "$@"; then
  printf '%s\n' '--- ok: package-deb'
  find target/debian -maxdepth 1 -name '*.deb' 2>/dev/null | tail -3
else
  printf '%s\n' '--- FAILED: package-deb' >&2
  exit 1
fi
