#!/usr/bin/env bash
# Profile-Guided Optimization build for the headless binary.
#
# PGO gives the compiler a real execution profile so it can lay out the
# hot VT-parse branches (CSI cursor-motion + SGR) for the common case.
# On top of the fat-LTO release profile it measured (idle host,
# `bench --mb 64 --iterations 3`, best-of-3, vs LTO-only):
#
#   case            LTO    +PGO     Δ
#   scrolling        62      66    +6%
#   sgr_dense       105     177   +68%
#   unicode          73      84   +15%
#   cursor_motion    27.5    40   +45%   <- was the slowest case
#   paste_random     69      76    +9%
#   mean             67      88   +31%
#
# The profiling workload is our own `bench` subcommand — it drives the
# exact PtyRing + alacritty parser path a live tab uses, so the profile
# is representative of real PTY ingest. (The gpui binary's paint path is
# NOT covered here; PGO-ing it would need a recorded interactive session
# as the workload — a future step.)
#
# Usage:
#   scripts/pgo-build.sh            # build target/release/tab-atelier-headless, PGO-optimized
#   PGO_MB=48 scripts/pgo-build.sh  # smaller profiling payload (faster)
#
# Requires the llvm-tools (`rustup component add llvm-tools-preview`).
# Not wired into the default `cargo build` because it needs that
# component + a profiling run; invoke it explicitly (or from the deb
# release pipeline) when you want the optimized binary.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

FEATURES="headless,energy"
PROF_DIR="${TMPDIR:-/tmp}/ta-pgo-data"
PROF_MERGED="${TMPDIR:-/tmp}/ta-pgo.profdata"
PGO_MB="${PGO_MB:-48}"

# Locate llvm-profdata inside the active rustup toolchain.
SYSROOT="$(rustc --print sysroot)"
PROFDATA="$(find "$SYSROOT" -name llvm-profdata -type f 2>/dev/null | head -1)"
if [[ -z "$PROFDATA" ]]; then
  echo "llvm-profdata not found — run: rustup component add llvm-tools-preview" >&2
  exit 1
fi

echo "==> 1/4 instrumented build"
rm -rf "$PROF_DIR" && mkdir -p "$PROF_DIR"
RUSTFLAGS="-Cprofile-generate=$PROF_DIR" \
  cargo build --release --no-default-features --features "$FEATURES"

echo "==> 2/4 generating profile (bench --mb $PGO_MB)"
./target/release/tab-atelier-headless bench --mb "$PGO_MB" --iterations 1 >/dev/null

echo "==> 3/4 merging profiles"
"$PROFDATA" merge -o "$PROF_MERGED" "$PROF_DIR"/*.profraw
echo "    merged: $(wc -c <"$PROF_MERGED") bytes"

echo "==> 4/4 PGO-optimized build"
RUSTFLAGS="-Cprofile-use=$PROF_MERGED -Cllvm-args=-pgo-warn-missing-function" \
  cargo build --release --no-default-features --features "$FEATURES"

echo "==> done: target/release/tab-atelier-headless (LTO + PGO)"
echo "    verify with: ./target/release/tab-atelier-headless bench"
