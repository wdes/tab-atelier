#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# mobile-apk.sh — build or check the Android app.
#
# Goes through cargo-apk2, never `cargo check --target aarch64-linux-android`:
# that fails in the Slint android-activity build script, which only apk2 sets
# up correctly.
#
#   scripts/mobile-apk.sh check    # compile-check, the fast one
#   scripts/mobile-apk.sh build    # APK (default)
#
# The SDK is found from ANDROID_HOME, defaulting to this machine's location; the
# NDK is the newest one installed under it unless ANDROID_NDK_ROOT says
# otherwise.
#
# Exit: 0 the command succeeded, 1 it failed or no NDK was found.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
# shellcheck source=scripts/lib/toolchain.sh
. scripts/lib/toolchain.sh

export ANDROID_HOME="${ANDROID_HOME:-/mnt/Glacier/AndroidSDK}"

if [ -z "${ANDROID_NDK_ROOT:-}" ]; then
  # sort -V so 26.1 beats 25.1 rather than losing to it lexically.
  ANDROID_NDK_ROOT="$(find "$ANDROID_HOME/ndk" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -V | tail -1)"
  export ANDROID_NDK_ROOT
fi

if [ ! -d "$ANDROID_NDK_ROOT" ]; then
  printf 'mobile-apk: no NDK under %s (set ANDROID_NDK_ROOT)\n' "$ANDROID_HOME" >&2
  exit 1
fi

printf '=== cargo apk2 %s (NDK %s)\n' "${1:-build}" "$(basename "$ANDROID_NDK_ROOT")"
cd android/ta-remote || exit 1
cargo apk2 "${1:-build}"
