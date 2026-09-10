#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Keep the Android app's version tied to tab-atelier's, and print the
# Android versionCode for a build.
#
#     scripts/android-version.sh            # show what it would use
#     scripts/android-version.sh --write    # sync android/ta-remote/Cargo.toml
#     scripts/android-version.sh --code     # print just the versionCode
#
# WHY A SEPARATE CODE. Android has two version fields and they are not the
# same kind of thing:
#
#   versionName  a display string. Anything goes — "0.5.0-dev" is fine, and it
#                is what a human sees. We mirror the workspace version.
#   versionCode  a 32-bit INT that must strictly increase with every artifact
#                the store or the device ever sees. It is the upgrade
#                ordering, and nothing else.
#
# A semver-derived code (major*10000 + minor*100 + patch) cannot work here:
# nightlies publish several artifacts a day under the SAME 0.5.0, so the code
# would repeat and the store would reject the upload — or worse, a device
# would refuse the upgrade silently.
#
# So the code is MINUTES SINCE 2020-01-01 UTC:
#
#   * strictly increasing for any two builds a minute apart, with no state to
#     keep and no counter to reset;
#   * ~3.5 million today, and it does not reach the 2 147 483 647 int32 limit
#     until the year 6000, so there is no overflow to plan for;
#   * decode a code back to a build time with:
#       date -u -d "@$(( $(date -u -d '2020-01-01' +%s) + CODE * 60 ))"
#
# Human-readable date codes (YYMMDDHH) were the alternative and were rejected:
# they only allow one build per hour, and two nightlies in the same hour would
# collide exactly when you least want to think about it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$root/android/ta-remote/Cargo.toml"

# The workspace version is the single source of truth for the NAME.
version_name="$(grep -m1 '^version = ' "$root/Cargo.toml" | cut -d'"' -f2)"
[ -n "$version_name" ] || { echo "$0: no version in $root/Cargo.toml" >&2; exit 1; }

epoch=$(date -u -d '2020-01-01 00:00:00' +%s)
now=$(date -u +%s)
version_code=$(( (now - epoch) / 60 ))

case "${1:-}" in
    --code)
        echo "$version_code"
        exit 0
        ;;
    --write)
        # Only the [package] version line, not the dependency versions below it.
        tmp="$(mktemp)"
        awk -v v="$version_name" '
            /^\[/ { in_pkg = ($0 == "[package]") }
            in_pkg && /^version = / { print "version = \"" v "\""; next }
            { print }
        ' "$manifest" > "$tmp"
        mv "$tmp" "$manifest"
        echo "$0: android/ta-remote version = $version_name (code $version_code)"
        ;;
    ''|--show)
        current="$(awk '/^\[/ { p = ($0 == "[package]") } p && /^version = / { print; exit }' "$manifest" | cut -d'"' -f2)"
        echo "workspace:      $version_name"
        echo "android crate:  $current"
        echo "versionCode:    $version_code   ($(date -u -d "@$now" '+%Y-%m-%d %H:%M UTC'))"
        [ "$current" = "$version_name" ] || echo "OUT OF SYNC — run: $0 --write" >&2
        ;;
    *)
        echo "usage: $0 [--show|--write|--code]" >&2
        exit 2
        ;;
esac
