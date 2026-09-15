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
  printf '%s\n' '--- ok: bundle in sync'
else
  printf '%s\n\n%s\n' '--- FAILED: bundle in sync' 'web-build: FAILED' >&2
  exit 1
fi

# The bundle being in sync says the committed output matches the source. It says
# nothing about whether the page runs: a method in the wrong Vue section
# compiles, passes tsc, and throws while rendering, which leaves a blank page
# and no clue. Rendering it is the only way to find that, so it is a build step
# rather than an optional extra.
#
# `node` explicitly, not the $runner above: bun and npm both proxy to node here,
# but naming it keeps the dependency visible, and a missing runtime must fail
# loudly. A guard that quietly skips is how the blank page shipped in the first
# place.
printf '\n=== dashboard renders\n'
if ! command -v node >/dev/null 2>&1; then
  printf '%s\n\n%s\n' '--- FAILED: node is not on PATH; the render check cannot run' 'web-build: FAILED' >&2
  exit 1
fi
if (./crates/tab-atelier-proxy/web/smoke.mjs); then
  printf '%s\n\n%s\n' '--- ok: dashboard renders' 'web-build: ok'
else
  printf '%s\n\n%s\n' '--- FAILED: dashboard renders' 'web-build: FAILED' >&2
  exit 1
fi
