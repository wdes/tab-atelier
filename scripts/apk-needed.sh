#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Decide whether the Android APK has to be rebuilt for this event.
# Prints `true` or `false`, and its reasoning on stderr.
#
#     scripts/apk-needed.sh                  # ask GitHub what the push changed
#     printf 'overlay/x\n' | scripts/apk-needed.sh --files -   # judge a list
#
# WHY THIS IS NOT `on.push.paths`. Two reasons, and the second is the one that
# bites:
#
#   1. It would also have to list the workflow file, which is easy to forget.
#   2. GitHub evaluates path filters against the COMMITS in the push. A tag
#      points at a commit that was already pushed, so the tag push carries no
#      commits, so no path ever matches — a plain `paths:` filter silently
#      skips the build.
#
# WHAT COUNTS, now that the app is a fork. The app is upstream ConnectBot at a
# pinned commit, plus the overlay we lay over it. So the inputs are:
#
#   connectbot            the submodule PIN — a gitlink, so a bump shows up as
#                         a one-line diff against this path and nothing else.
#                         Upstream code changed, so the APK changed.
#   overlay/              our own changes: patches, copied-in files, deletions.
#   scripts/*.sh          the build (build-apk applies the overlay, publish-apk
#                         stages the site). Editing the build is a reason to
#                         run it.
#   the workflow itself   same reasoning.
#
# Everything else — this repository's daemon, docs, the deb packaging — cannot
# reach the APK, which is what makes skipping safe.
#
# WHEN IN DOUBT, BUILD. Every uncertain branch here answers `true`. A needless
# five-minute build costs a runner; a wrongly skipped one means the site serves
# an APK that does not match the commit it claims to be.
set -euo pipefail

# Paths whose contents end up in, or shape, the APK.
matches_app() {
    grep -qE '^(connectbot$|overlay/|scripts/(build-apk|apply-overlay|publish-apk|apk-needed)\.sh$|\.github/workflows/android-apk\.yml$)'
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
    say false "nothing that reaches the APK changed"
fi

case "${GITHUB_REF:-}" in
    # A tagged build must produce an APK whatever the diff says.
    refs/tags/*) say true "tag build" ;;
esac
[ "${GITHUB_EVENT_NAME:-push}" = push ] || say true "${GITHUB_EVENT_NAME} run — not a push"

# A push to the branch: ask GitHub which files it changed. `before` is all
# zeroes on a new branch's first push, and on a force-push it may not be an
# ancestor, so both cases fall back to building.
before="${GITHUB_EVENT_BEFORE:-}"
if [ -z "$before" ] || [ "${before//0/}" = "" ] || ! git cat-file -e "$before^{commit}" 2>/dev/null; then
    say true "cannot diff the push (new branch or force-push) — building"
fi

files="$(gh api "repos/${GITHUB_REPOSITORY}/compare/${before}...${GITHUB_SHA}" \
    --jq '.files[].filename' 2>/dev/null)" || say true "could not ask GitHub for the diff — building"

if printf '%s\n' "$files" | matches_app; then
    say true "the app changed"
fi
say false "no file that reaches the APK changed"
