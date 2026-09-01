#!/usr/bin/env bash
#
# outbox-sweep.test.sh — KIOSK PD5 real-fs test (anti-built≠wired, NO mock).
#
# Builds a temp outbox + a temp decision log with one OPEN decision referencing a real
# file, then runs outbox-sweep.sh (dry-run) against them and asserts the three cases:
#   (a) an ORPHAN older than N days, unreferenced          -> LISTED as a candidate
#   (b) a file REFERENCED by the open decision             -> SPARED (never listed)
#   (c) a file whose first line is `# keep`                -> SPARED (never listed)
# plus: the DRY-RUN moves NOTHING (all files intact, no _archive created).
#
# Uses the freshly-built headless binary (the installed `tab-atelier` predates the
# `decision` subcommand). Run: bash scripts/outbox-sweep.test.sh

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
sweep="$here/outbox-sweep.sh"
bin="$here/../target/debug/tab-atelier-headless"
[ -x "$bin" ] || { echo "SKIP: $bin not built (run: cargo build --no-default-features --features headless)"; exit 0; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
outbox="$tmp/outbox"
log="$tmp/decisions.jsonl"
mkdir -p "$outbox"

orphan="$outbox/orphan.md"      # old, unreferenced        -> candidate
ref="$outbox/referenced.md"     # old, referenced by open  -> spared
keep="$outbox/pinned.md"        # old, `# keep`            -> spared
printf 'stale orphan bundle\n'      > "$orphan"
printf 'a live referenced bundle\n' > "$ref"
printf '# keep\nnever sweep me\n'   > "$keep"
# age all three well past the 14-day default
touch -d '30 days ago' "$orphan" "$ref" "$keep"

# one OPEN decision that references `ref` (so the sweep must spare it)
TAB_ATELIER_DECISIONS_PATH="$log" "$bin" decision push \
    --id kiosk-pd5 --project harness --title "PD5 demo" --files "$ref" >/dev/null

out="$(TAB_ATELIER_OUTBOX_PATH="$outbox" TAB_ATELIER_DECISIONS_PATH="$log" TAB_ATELIER_BIN="$bin" \
    bash "$sweep" --days 14)"

fail=0
assert() { # <desc> <cond-cmd...>
    local desc="$1"; shift
    if "$@"; then echo "  ok  — $desc"; else echo "  FAIL — $desc"; fail=1; fi
}
lists()   { printf '%s\n' "$out" | grep -Fq "$1"; }

echo "$out"
echo "--- assertions ---"
assert "(a) orphan LISTED as candidate"        lists "$orphan"
assert "(b) referenced-by-open file SPARED"    bash -c '! grep -Fq "$0" <<<"$1"' "$ref" "$out"
assert "(c) '\''# keep'\'' file SPARED"        bash -c '! grep -Fq "$0" <<<"$1"' "$keep" "$out"
# dry-run must not have touched the disk
assert "dry-run moved NOTHING (files intact)"  bash -c '[ -f "$1" ] && [ -f "$2" ] && [ -f "$3" ]' _ "$orphan" "$ref" "$keep"
assert "dry-run created NO _archive"           bash -c '[ ! -d "$1/_archive" ]' _ "$outbox"

if [ "$fail" -eq 0 ]; then
    echo "OK — outbox-sweep 3-case test GREEN"
else
    echo "TEST FAILED"; exit 1
fi
