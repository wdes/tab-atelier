#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the canonical documentation set into a directory.
#
#     scripts/install-docs.sh "$pkgdir/usr/share/doc/tab-atelier"
#
# The list lives in packaging/docs.list so the Arch PKGBUILD and the deb's
# asset table cannot drift apart unnoticed — a Rust test asserts every entry
# here is also in Cargo.toml.
set -euo pipefail

dest="${1:-}"
if [ -z "$dest" ]; then
    echo "usage: $0 <destination-directory>" >&2
    exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
list="$root/packaging/docs.list"
[ -f "$list" ] || { echo "$0: missing $list" >&2; exit 1; }

install -d "$dest"
count=0
while IFS= read -r line; do
    # Skip blanks and comments.
    case "$line" in ''|\#*) continue ;; esac
    src="$root/$line"
    if [ ! -f "$src" ]; then
        # A listed file that does not exist is a packaging bug, not something
        # to install around: the package would silently ship less than it says.
        echo "$0: listed but missing: $line" >&2
        exit 1
    fi
    install -Dm644 "$src" "$dest/$(basename "$line")"
    count=$((count + 1))
done < "$list"

echo "$0: installed $count doc(s) into $dest"
