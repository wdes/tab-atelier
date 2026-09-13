#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# web-build.sh — rebuild the dashboard bundle and prove it is in sync.
#
# The dashboard is Vue's global build with no bundler: app.ts is compiled by tsc
# into assets/app.js, which is committed, because the binary includes it with
# include_str! at compile time. Edit the TypeScript without running this and the
# committed bundle is stale -- and `tests/web_assets.rs` fails, which is what
# the second step here checks.
#
#   scripts/web-build.sh
#
# Seconds of work: safe to run inline.
#
# Exit: 0 bundle rebuilt and in sync, 1 either step failed.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

if command -v bun >/dev/null 2>&1; then
  runner=bun
elif command -v npm >/dev/null 2>&1; then
  runner=npm
else
  echo 'web-build: neither bun nor npm is on PATH' >&2
  exit 1
fi

printf '=== tsc (%s)\n' "$runner"
if ! (cd crates/tab-atelier-proxy/web && "$runner" run build); then
  printf '%s\n' '--- FAILED: tsc' >&2
  exit 1
fi
printf '%s\n' '--- ok: tsc'

printf '\n=== bundle in sync\n'
if cargo test -p tab-atelier-proxy --test web_assets; then
  printf '%s\n\n%s\n' '--- ok: bundle in sync' 'web-build: ok'
else
  printf '%s\n\n%s\n' '--- FAILED: bundle in sync' 'web-build: FAILED' >&2
  exit 1
fi
