#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Print the Debian version for the package being built here, right now.
#
#     scripts/deb-version.sh                 # 0.5.0~nightly20260907070700.04cb329-1
#     scripts/deb-version.sh --revision 0ci  # …-0ci, for a throwaway build
#     scripts/deb-version.sh --explain       # the components, one per line
#
# Feed it to cargo-deb:
#
#     cargo deb -p tab-atelier --locked --deb-version "$(scripts/deb-version.sh)"
#
# THE SHAPE, and why. Debian's convention for snapshots (Debian Policy 5.6.12.2,
# and the "Versioning in Nightly and Prerelease Binaries" wiki page) is
#
#     {upcoming_version}~git{date}.{hash}-{revision}
#
# We keep every part of that but spell the marker `nightly` instead of `git`,
# because it says what the package IS to the person reading the filename — the
# nightly channel — where `git` only says how it was identified. dpkg does not
# care which word it is: what matters is the `~`.
#
# `~` sorts BEFORE an empty string in dpkg's comparison, and before nothing
# else does. That single character is what makes a nightly for 0.5.0 sort below
# the eventual 0.5.0 release, so a machine tracking `nightly` steps up onto
# stable when it lands rather than being pinned above it forever.
#
#     0.5.0-1                            the release
#     0.5.0~pre2-1                       a release candidate for it
#     0.5.0~nightly20260907070700.abc-1  a nightly heading towards it
#     0.3.0-1                            the previous release
#
# WHY THE DATE IS 14 DIGITS AND NOT THE WIKI'S 8. The wiki writes the date as
# YYYYMMDD because it assumes one nightly per day. We publish on every push to
# main, several a day, and the hash CANNOT order them: dpkg compares it as
# text, so `abc1234` vs `0de5678` sorts by the alphabet and not by time. Two
# pushes on one day would produce versions in a random order, and apt on a
# nightly machine would refuse the second one as a downgrade. The date field
# therefore carries the time too — YYYYMMDDHHMMSS — which is the convention's
# intent ("the date part … will be used to compare versions in an incremental
# way") applied to a repo that builds more than daily. The hash stays exactly
# what the wiki says it is: informational, so a .deb on your disk can name the
# commit it came from.
#
# Keeping `~nightly` also costs nothing to switch to: the old suffix was
# `~nightly{YYYYMMDD}.{HHMMSS}`, and 14 digits compare numerically GREATER than
# 8, so every version minted here outranks every one already published. No
# machine has to be told to downgrade.
#
# REVISION. `-1` is the real thing, published to the apt repo. The build
# workflow's smoke-test debs pass `--revision 0ci`: same commit, but built with
# LTO off and never published, and `0ci` sorts below `1` so it can never
# shadow the published build of the same commit.
#
# SOURCE_DATE_EPOCH is honoured if set, so re-running a build can reproduce a
# version instead of minting a new one.
set -euo pipefail

revision=1
explain=0
want_base=0
while [ $# -gt 0 ]; do
    case "$1" in
        --revision) revision="${2:?--revision needs a value}"; shift 2 ;;
        --explain)  explain=1; shift ;;
        --base)     want_base=1; shift ;;
        -h|--help)  sed -n '6,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "$0: unknown argument: $1" >&2; exit 2 ;;
    esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The upstream version, from the one place that holds it.
raw="$(grep -m1 '^version = ' "$root/Cargo.toml" | cut -d'"' -f2)"
[ -n "$raw" ] || { echo "$0: no version in $root/Cargo.toml" >&2; exit 1; }

# `0.5.0-dev` means "0.5.0 is what we are heading for" — that is the upcoming
# version the convention asks for, so the marker comes off. Any OTHER cargo
# pre-release (`0.5.0-rc1`) keeps its identity but switches to Debian's
# separator, since `-` would read as a revision and sort the wrong way.
base="${raw%-dev}"
case "$base" in
    *-*) base="${base%%-*}~${base#*-}" ;;
esac

# A tag is the release itself: no snapshot suffix, nothing sorting below it.
if [ -n "${GITHUB_REF:-}" ] && [ "${GITHUB_REF#refs/tags/v}" != "${GITHUB_REF}" ]; then
    channel=stable
    base="${GITHUB_REF#refs/tags/v}"
    case "$base" in
        *-*) base="${base%%-*}~${base#*-}" ;;
    esac
else
    channel=nightly
fi

# Windows asks for a bare numeric x.y.z: an MSI ProductVersion has no room for
# a pre-release marker, and the installers used to hardcode one that went stale.
# Answered before anything reaches for `date` or `git`, because this runs under
# git-bash on the Windows runners.
if [ "$want_base" = 1 ]; then
    echo "${base%%\~*}"
    exit 0
fi

if [ "$channel" = stable ]; then
    version="${base}-${revision}"
else
    stamp="$(date -u -d "@${SOURCE_DATE_EPOCH:-$(date -u +%s)}" +%Y%m%d%H%M%S)"
    # The checked-out tree is what we built; GITHUB_SHA is the fallback for a
    # tarball build with no .git around it.
    hash="$(git -C "$root" rev-parse --short=7 HEAD 2>/dev/null || echo "${GITHUB_SHA:-0000000}")"
    hash="${hash:0:7}"
    version="${base}~nightly${stamp}.${hash}-${revision}"
fi

if [ "$explain" = 1 ]; then
    printf 'cargo version:   %s\n' "$raw"
    printf 'upstream base:   %s\n' "$base"
    printf 'channel:         %s\n' "$channel"
    printf 'revision:        %s\n' "$revision"
    printf 'deb version:     %s\n' "$version"
    exit 0
fi

echo "$version"
