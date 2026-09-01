#!/usr/bin/env bash
#
# outbox-sweep.test.sh — KIOSK PD5/PD5b real-fs test (anti-built≠wired, NO mock).
#
# Three scenarios, all against a temp outbox + temp decision log + the freshly-built
# headless binary (the installed `tab-atelier` predates the `decision` subcommand):
#   1. DRY-RUN  : orphan LISTED; open-referenced SPARED; `# keep` SPARED; nothing moved.
#   2. --APPLY  : orphan ARCHIVED; open-referenced AND read-referenced SPARED (PD5b: a
#                 read-but-not-ruled decision still needs its bundle).
#   3. FAIL-CLOSED (PD5b): when `decision list` fails, the sweep ABORTS and moves NOTHING
#                 rather than sweeping against an empty reference set.
# Run: bash scripts/outbox-sweep.test.sh

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
sweep="$here/outbox-sweep.sh"
bin="$here/../target/debug/tab-atelier-headless"
[ -x "$bin" ] || { echo "SKIP: $bin not built (run: cargo build --no-default-features --features headless)"; exit 0; }

fail=0
ok()   { echo "  ok  — $1"; }
bad()  { echo "  FAIL — $1"; fail=1; }
absent_from() { ! grep -Fq "$1" <<<"$2"; }   # $1 not in text $2

# --- Scenario 1: DRY-RUN ---------------------------------------------------------------
s1="$(mktemp -d)"; ob="$s1/outbox"; log="$s1/d.jsonl"; mkdir -p "$ob"
orphan="$ob/orphan.md"; ref="$ob/referenced.md"; keep="$ob/pinned.md"
printf 'stale orphan\n' > "$orphan"; printf 'referenced\n' > "$ref"; printf '# keep\nx\n' > "$keep"
touch -d '30 days ago' "$orphan" "$ref" "$keep"
TAB_ATELIER_DECISIONS_PATH="$log" "$bin" decision push --id d1 --project harness --title t --files "$ref" >/dev/null
out="$(TAB_ATELIER_OUTBOX_PATH="$ob" TAB_ATELIER_DECISIONS_PATH="$log" TAB_ATELIER_BIN="$bin" bash "$sweep" --days 14)"
echo "[1 dry-run] $out"
grep -Fq "$orphan" <<<"$out" && ok "(a) orphan LISTED"                 || bad "(a) orphan LISTED"
absent_from "$ref"  "$out" && ok "(b) open-referenced SPARED"          || bad "(b) open-referenced SPARED"
absent_from "$keep" "$out" && ok "(c) '# keep' SPARED"                 || bad "(c) '# keep' SPARED"
[ -f "$orphan" ] && [ -f "$ref" ] && [ -f "$keep" ] && [ ! -d "$ob/_archive" ] \
    && ok "(d) dry-run moved NOTHING" || bad "(d) dry-run moved NOTHING"
rm -rf "$s1"

# --- Scenario 2: --APPLY (open AND read spared) ---------------------------------------
s2="$(mktemp -d)"; ob="$s2/outbox"; log="$s2/d.jsonl"; mkdir -p "$ob"
orphan="$ob/orphan.md"; refo="$ob/ref-open.md"; refr="$ob/ref-read.md"
printf 'orphan\n' > "$orphan"; printf 'open ref\n' > "$refo"; printf 'read ref\n' > "$refr"
touch -d '30 days ago' "$orphan" "$refo" "$refr"
TAB_ATELIER_DECISIONS_PATH="$log" "$bin" decision push --id o --project harness --title t --files "$refo" >/dev/null
TAB_ATELIER_DECISIONS_PATH="$log" "$bin" decision push --id r --project harness --title t --files "$refr" >/dev/null
TAB_ATELIER_DECISIONS_PATH="$log" "$bin" decision read --id r >/dev/null   # r is now state=read
TAB_ATELIER_OUTBOX_PATH="$ob" TAB_ATELIER_DECISIONS_PATH="$log" TAB_ATELIER_BIN="$bin" bash "$sweep" --days 14 --apply >/dev/null
[ ! -f "$orphan" ] && [ -f "$ob/_archive/$(date -u +%Y-%m)/orphan.md" ] \
    && ok "(e) --apply ARCHIVED the orphan (moved, not rm)" || bad "(e) --apply ARCHIVED the orphan"
[ -f "$refo" ] && ok "(f) --apply SPARED the open-referenced file"  || bad "(f) --apply SPARED the open-referenced file"
[ -f "$refr" ] && ok "(g) --apply SPARED the read-referenced file (PD5b)" || bad "(g) --apply SPARED the read-referenced file"
rm -rf "$s2"

# --- Scenario 3: FAIL-CLOSED (decision list fails → abort, nothing moved) --------------
s3="$(mktemp -d)"; ob="$s3/outbox"; mkdir -p "$ob"
orphan="$ob/orphan.md"; printf 'orphan\n' > "$orphan"; touch -d '30 days ago' "$orphan"
rc=0
TAB_ATELIER_OUTBOX_PATH="$ob" TAB_ATELIER_BIN=false bash "$sweep" --days 14 --apply >/dev/null 2>&1 || rc=$?
[ "$rc" -eq 4 ] && ok "(h) fail-closed: aborted (exit 4) when 'decision list' fails" || bad "(h) fail-closed exit 4 (got $rc)"
[ -f "$orphan" ] && [ ! -d "$ob/_archive" ] \
    && ok "(i) fail-closed: moved NOTHING" || bad "(i) fail-closed moved NOTHING"
rm -rf "$s3"

if [ "$fail" -eq 0 ]; then
    echo "OK — outbox-sweep test GREEN (dry-run + --apply open/read spare + fail-closed)"
else
    echo "TEST FAILED"; exit 1
fi
