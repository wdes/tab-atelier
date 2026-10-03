#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Lay our changes over the pinned upstream ConnectBot checkout.
#
# Debian-style: modifications to upstream files are quilt-style patches in
# patches/, and files that do not exist upstream (or replace one wholesale) are
# in files/, copied in verbatim. Nothing here is ever committed inside the
# submodule, so `git -C connectbot checkout .` always returns it to upstream's
# tree and the patches are the single record of what we changed.
#
#   scripts/apply-overlay.sh           apply
#   scripts/apply-overlay.sh --check   report whether it still applies, change nothing
#
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="$root/connectbot"
overlay="$root/overlay"
check=false

case "${1:-}" in
    --check) check=true ;;
    "") ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

if [[ ! -f "$upstream/settings.gradle.kts" ]]; then
    echo "connectbot/ is not checked out." >&2
    echo "Run: git submodule update --init" >&2
    exit 1
fi

# The submodule must be pristine. If it is not, a patch would apply on top of
# an unknown tree and the reason a build failed would be invisible. Untracked
# files are ignored: those are the copies in files/ from a previous apply, and
# they are ours, not a sign the tree drifted.
if [[ -n "$(git -C "$upstream" status --porcelain --untracked-files=no)" ]]; then
    if [[ "$check" == true ]]; then
        echo "NOTE: connectbot/ has local changes — upstream drift, or the overlay" >&2
        echo "      is already applied. Comparing against this tree, not pristine upstream." >&2
    else
        echo "connectbot/ has local changes; refusing to patch an unknown tree." >&2
        echo "Reset it first: git -C connectbot checkout -- ." >&2
        exit 1
    fi
fi

fail=0
for patch in "$overlay"/patches/*.patch; do
    [[ -e "$patch" ]] || continue
    name="$(basename "$patch")"
    if git -C "$upstream" apply --check "$patch" 2>/dev/null; then
        if [[ "$check" == true ]]; then
            echo "ok       $name"
        else
            git -C "$upstream" apply "$patch"
            echo "applied  $name"
        fi
    elif git -C "$upstream" apply --reverse --check "$patch" 2>/dev/null; then
        echo "already  $name (already applied)"
    else
        # Upstream moved under us. Name the files so the fix is a rebase of one
        # patch, not a hunt through the tree.
        echo "CONFLICT $name" >&2
        git -C "$upstream" apply --check "$patch" 2>&1 | sed 's/^/         /' >&2 || true
        fail=1
    fi
done

if [[ -f "$overlay/remove.txt" ]]; then
    while IFS= read -r rel; do
        [[ -z "$rel" || "$rel" == \#* ]] && continue
        if [[ -e "$upstream/$rel" ]]; then
            if [[ "$check" == true ]]; then
                echo "removes  $rel"
            else
                # ${upstream:?} would abort on an empty upstream, which would
                # otherwise turn this into `rm -rf /$rel`. ${rel:?} likewise.
                rm -rf "${upstream:?}/${rel:?}"
                echo "removed  $rel"
            fi
        fi
    done < "$overlay/remove.txt"
fi

if [[ -d "$overlay/files" ]]; then
    while IFS= read -r -d '' file; do
        rel="${file#"$overlay/files/"}"
        if [[ "$check" == true ]]; then
            echo "copies   $rel"
        else
            mkdir -p "$(dirname "$upstream/$rel")"
            cp "$file" "$upstream/$rel"
            echo "copied   $rel"
        fi
    done < <(find "$overlay/files" -type f -print0)
fi

if [[ "$fail" != 0 ]]; then
    echo >&2
    echo "One or more patches did not apply. The submodule pin moved: rebase the" >&2
    echo "patch named above onto the new upstream tree." >&2
    exit 1
fi

[[ "$check" == true ]] && echo "overlay still applies cleanly"
exit 0
