#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Fetch the pinned Vue build the proxy's admin UI is served with, and commit
# the result. Run once; it is vendored, not downloaded at build time.
#
#     scripts/fetch-vue.sh
#     scripts/fetch-vue.sh --print-sha   # what the current file hashes to
#
# WHY VENDORED. A credential proxy is exactly the sort of thing that runs on a
# network with no outbound access, where a CDN script tag means the admin UI
# does not load at all. It would also give a third party a script tag on the
# page where the admin token is typed.
#
# Bootstrap is NOT fetched here: Debian packages it (libjs-bootstrap5) and the
# .deb depends on that, so it stays patched without us re-vendoring it.
set -euo pipefail

VERSION="3.5.13"
URL="https://unpkg.com/vue@${VERSION}/dist/vue.global.prod.js"
# Pin the exact bytes. An unpinned vendored dependency is just a slow CDN: the
# point of committing it is that what ships is what was reviewed.
#
# Pinned from the bytes actually vendored (bun add vue@3.5.13, then sha256sum
# on the file in assets/vendor/). Re-derive with --print-sha after a bump.
SHA256="c459ba7cc8db65c982589fa5d64c7ff478877e8e5b0fd75683207cec6a4e89e8"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/crates/tab-atelier-proxy/assets/vendor/vue.global.prod.js"

if [ "${1:-}" = "--print-sha" ]; then
    [ -f "$dest" ] || { echo "$0: not fetched yet: $dest" >&2; exit 1; }
    sha256sum "$dest"
    exit 0
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

echo "$0: fetching Vue $VERSION" >&2
# bun first: it reaches npm without any of the proxy/allowlist arrangements a
# raw CDN fetch needs on some machines, and it is how the committed copy was
# obtained.
if command -v bun >/dev/null 2>&1; then
    work="$(mktemp -d)"
    trap 'rm -rf "$work" "$tmp"' EXIT
    (cd "$work" && bun add "vue@${VERSION}" >/dev/null 2>&1)
    cp "$work/node_modules/vue/dist/vue.global.prod.js" "$tmp"
elif command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$tmp"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$tmp" "$URL"
else
    echo "$0: need bun, curl or wget" >&2
    exit 1
fi

got="$(sha256sum "$tmp" | cut -d' ' -f1)"
if [ -z "$SHA256" ]; then
    echo "$0: no pin recorded yet. Downloaded Vue $VERSION hashes to:" >&2
    echo "    $got" >&2
    echo "  Check it against https://unpkg.com/vue@${VERSION}/dist/ then set" >&2
    echo "  SHA256 in this script, so later fetches verify against it." >&2
elif [ "$got" != "$SHA256" ]; then
    # Either the pin is stale (a version bump nobody updated here) or the bytes
    # are not what we expect. Both mean: do not write this into the tree.
    echo "$0: checksum mismatch for Vue $VERSION" >&2
    echo "  expected $SHA256" >&2
    echo "  got      $got" >&2
    echo "  If you are deliberately bumping the version, update SHA256 in this script." >&2
    exit 1
fi

mkdir -p "$(dirname "$dest")"
cp "$tmp" "$dest"
echo "$0: wrote $dest ($(wc -c < "$dest") bytes)" >&2
echo "$0: commit it — the .deb ships this file." >&2
