#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Decide whether the Android APK has to be rebuilt for this event.
# Prints `true` or `false`, and its reasoning on stderr.
#
#     scripts/apk-needed.sh                  # ask GitHub what the push changed
#     printf 'android/x\n' | scripts/apk-needed.sh --files -   # judge a list
#
# WHY THIS IS NOT `on.push.paths`. Two reasons, and the second is the one that
# bites:
#
#   1. It would also have to list the workflow file, which is easy to forget.
#   2. GitHub evaluates path filters against the COMMITS in the push. A tag
#      points at a commit that was already pushed, so the tag push carries no
#      commits, so no path ever matches — a plain `paths:` filter silently
#      skips every release build. The APK for v1.2.3 would just never exist.
#
# So tags (and manual runs) build unconditionally, and only a push to a branch
# is judged on its diff.
#
# WHAT COUNTS. android/ta-remote is deliberately outside the desktop workspace
# — its own [workspace], its own lockfile, a disjoint dependency tree, and no
# path dependency on the desktop crate. Nothing under src/ can change the APK,
# which is what makes skipping safe. The workflow file counts too: editing the
# build is a reason to run it.
#
# WHEN IN DOUBT, BUILD. Every uncertain branch here answers `true`. A needless
# 90-second build costs a runner minute; a wrongly skipped one means the site
# serves an APK that does not match the release it claims to be.
set -euo pipefail

# Paths whose contents end up in, or shape, the APK.
matches_app() {
    grep -qE '^(android/|\.github/workflows/android-apk\.yml$|scripts/apk-needed\.sh$)'
}

say() {
    echo "$1"
    echo "apk-needed: $2" >&2
    exit 0
}

if [ "${1:-}" = "--files" ]; then
    # Judge a file list handed to us (the test seam, and a way to check a
    # decision by hand). `-` means stdin.
    src="${2:?--files needs a path or -}"
    if [ "$src" = "-" ]; then list="$(cat)"; else list="$(cat "$src")"; fi
    if printf '%s\n' "$list" | matches_app; then
        say true "the app changed"
    fi
    say false "nothing under android/ changed"
fi

case "${GITHUB_REF:-}" in
    # A release must have an APK regardless of what the diff says.
    refs/tags/*) say true "tag build" ;;
esac
[ "${GITHUB_EVENT_NAME:-push}" = push ] || say true "${GITHUB_EVENT_NAME} run — not a push"

before="${GITHUB_EVENT_BEFORE:-}"
case "$before" in
    # First push of a branch: no baseline to diff against.
    '' | 0000000000000000000000000000000000000000) say true "no baseline commit to compare with" ;;
esac

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
sha="${GITHUB_SHA:?GITHUB_SHA is required}"
resp="$(gh api "repos/$repo/compare/$before...$sha" 2>/dev/null || true)"
[ -n "$resp" ] || say true "could not read the push diff — building rather than guessing"

# The compare API caps `files` at 300. A push that big is not one we can judge.
count="$(printf '%s' "$resp" | jq '.files | length // 0')"
[ "$count" -lt 300 ] || say true "diff truncated at 300 files — building rather than guessing"

if printf '%s' "$resp" | jq -r '.files[].filename' | matches_app; then
    say true "the app changed in $before..$sha"
fi
say false "nothing under android/ changed in $before..$sha"
