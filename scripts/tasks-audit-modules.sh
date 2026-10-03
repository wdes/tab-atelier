#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# A backlog source: one audit task per module.
#
#   tab-atelier backlog --from ./scripts/tasks-audit-modules.sh
#
# Prints `id<TAB>title` lines, which is the whole contract a source has to
# meet. Emitting the entire tree every run is fine and expected — `backlog`
# skips ids already open or finished within the cooldown, so a source never
# has to remember what it said last time.
#
# Ordered largest-first only for readability; `take` decides who does what.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

find src crates -name '*.rs' -type f 2>/dev/null \
    | grep -v '/target/' \
    | while read -r f; do
        lines=$(wc -l < "$f")
        # Below ~80 lines a module is usually one small thing, and an audit
        # task costs an agent's whole context window either way.
        [ "$lines" -lt 80 ] && continue
        printf '%s\t%s\n' \
            "audit:$f" \
            "audit $f ($lines lines): one focused pass — correctness, error paths, anything surprising. Report findings with \`done\`; do not refactor."
    done | sort -t'(' -k2 -rn
