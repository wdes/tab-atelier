#!/usr/bin/env bash
#
# outbox-sweep.sh — KIOSK PD5: propose archiving ORPHAN outbox files (anti-entassement).
#
# Lists files in ~/Dev/outbox/ OLDER than N days (default 14, --days N) that are NOT
# referenced by any OPEN decision in the log, and proposes archiving them. DRY-RUN by
# default (writes NOTHING — just lists the candidates); --apply moves them to
# ~/Dev/outbox/_archive/AAAA-MM/ (consistent with PD3). NEVER `rm`s — always a move.
#
# This is a SCRIPT (opt-in), NOT a daemon/cron — coherent with the anti-proliferation
# stance (no cron de ménage). Run it by hand / on demand.
#
# ponytail: the "orphan" verdict is a HEURISTIC — file age (mtime) + reference by an
# `open` decision's files[]. It has NO semantic understanding of a file's importance.
# A file is NEVER swept if its FIRST line is `# keep`. Upgrade path: richer front-matter
# (tags, an explicit owner/expiry) or a per-file `.keep` sidecar, once the heuristic
# proves too coarse.
#
# Sources (env-overridable for testing):
#   TAB_ATELIER_OUTBOX_PATH     the outbox dir (default ~/Dev/outbox)
#   TAB_ATELIER_DECISIONS_PATH  the decision log (read via `tab-atelier decision list`)
#   TAB_ATELIER_BIN             the tab-atelier binary (default `tab-atelier`)
#
# Usage: outbox-sweep.sh [--days N] [--apply]

set -euo pipefail

days=14
apply=0
tabatelier="${TAB_ATELIER_BIN:-tab-atelier}"
outbox="${TAB_ATELIER_OUTBOX_PATH:-$HOME/Dev/outbox}"

while [ $# -gt 0 ]; do
    case "$1" in
        --days) days="$2"; shift 2 ;;
        --apply) apply=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "outbox-sweep: unexpected arg '$1'" >&2; exit 2 ;;
    esac
done

[ -d "$outbox" ] || { echo "outbox-sweep: no outbox at $outbox" >&2; exit 0; }
command -v jq >/dev/null 2>&1 || { echo "outbox-sweep: jq not found" >&2; exit 3; }

# The files to SPARE: every file referenced by an OPEN decision (state=open). A tilde is
# expanded to $HOME so the comparison matches the on-disk path. Newline-separated set.
spared="$(
    "$tabatelier" decision list 2>/dev/null \
        | jq -r '.[] | select(.state=="open") | .files[]?' \
        | sed "s|^~/|$HOME/|" \
        || true
)"

is_spared() {  # absolute path
    local f="$1"
    [ -n "$spared" ] || return 1
    printf '%s\n' "$spared" | grep -Fxq "$f"
}

has_keep() {  # a file whose FIRST line is `# keep` is never swept
    [ "$(head -n1 "$1" 2>/dev/null || true)" = "# keep" ]
}

# Candidate orphans: files under outbox (NOT under _archive/), older than N days, not
# referenced by an open decision, not `# keep`.
candidates=()
while IFS= read -r -d '' f; do
    is_spared "$f" && continue
    has_keep "$f" && continue
    candidates+=("$f")
done < <(find "$outbox" -type f -mtime "+$days" -not -path "*/_archive/*" -print0)

if [ "${#candidates[@]}" -eq 0 ]; then
    echo "outbox-sweep: no orphan candidates (> ${days}d, unreferenced, non-keep)."
    exit 0
fi

if [ "$apply" -eq 0 ]; then
    echo "outbox-sweep: DRY-RUN — ${#candidates[@]} orphan candidate(s) (> ${days}d, unreferenced). Re-run with --apply to archive:"
    printf '  %s\n' "${candidates[@]}"
    exit 0
fi

# --apply: move each orphan into _archive/AAAA-MM/ (PD3-consistent monthly bucket). Never rm.
month="$(date -u +%Y-%m)"
dest="$outbox/_archive/$month"
mkdir -p "$dest"
for f in "${candidates[@]}"; do
    mv -n "$f" "$dest/$(basename "$f")"
    echo "archived: $f -> $dest/$(basename "$f")"
done
