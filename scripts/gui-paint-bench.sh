#!/usr/bin/env bash
# GUI paint-loop benchmark harness.
#
# Launches a FULLY ISOLATED tab-atelier GUI instance (its own HOME →
# its own config + state + single-instance lock + ports), with the
# TAB_ATELIER_PAINT_LOG instrument on, so you can stress the paint
# loop in a real window and read the timings — WITHOUT touching your
# live instance's preferences/tabs.
#
# The headless `tab-atelier-headless bench` covers parse/ring
# throughput + allocations (display-free, CI-trackable); THIS covers
# the gpui paint loop (prepaint phase1/phase2 + actual GPU present
# rate), which can only be measured in a real window.
#
# Usage:
#   scripts/gui-paint-bench.sh [seconds]
#       Launches, prints the API base + token + tab id, waits
#       `seconds` (default: until you Ctrl-C) while you interact with
#       the window, then prints the captured paint log and tears down.
#
#   Drive output into the tab from another shell with the printed
#   token, e.g.:
#       curl -s -XPOST "$BASE/tabs/by-id/$TAB/input" \
#         -H "Authorization: Bearer $TOKEN" \
#         --data-binary $'/usr/games/piu-piu\r'
#
# Safety: your real ~/.config/tab-atelier is backed up to /tmp first
# and never written by this harness (the isolated instance uses
# HOME=$WORK so config_dir() resolves to $WORK/.config).

set -euo pipefail

SECONDS_TO_RUN="${1:-0}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/release/tab-atelier"
WORK="/tmp/ta-gui-bench-$$"
LOG="$WORK/paint.log"
API_PORT=7899
TLS_PORT=7900

if [[ ! -x "$BIN" ]]; then
  echo "build the release GUI first: cargo build --release" >&2
  exit 1
fi

# 1. Back up the REAL config+state (belt-and-suspenders; we don't write it).
TS="$(date +%Y%m%d-%H%M%S)"
BACKUP="/tmp/ta-config-backup-$TS.tar.gz"
tar czf "$BACKUP" -C "$HOME" .config/tab-atelier .local/state/tab-atelier 2>/dev/null || true
echo "backup of your real config → $BACKUP"

# 2. Isolated environment.
mkdir -p "$WORK/.config/tab-atelier"
cat > "$WORK/.config/tab-atelier/preferences.json" <<JSON
{ "api_addr": "127.0.0.1:$API_PORT", "api_tls_addr": "127.0.0.1:$TLS_PORT", "pty_cols": 200, "pty_rows": 50 }
JSON

cleanup() {
  [[ -n "${GUI_PID:-}" ]] && kill "$GUI_PID" 2>/dev/null || true
  sleep 1
  [[ -n "${GUI_PID:-}" ]] && kill -9 "$GUI_PID" 2>/dev/null || true
  echo
  echo "=== captured paint log ==="
  grep '^paint:' "$LOG" 2>/dev/null | tail -20 || echo "(no paint frames captured)"
  rm -rf "$WORK"
  echo "torn down; backup retained: $BACKUP"
}
trap cleanup EXIT INT TERM

# 3. Launch isolated with the paint instrument.
env -u XDG_STATE_HOME -u XDG_CONFIG_HOME \
  HOME="$WORK" TAB_ATELIER_PAINT_LOG=1 \
  "$BIN" >"$LOG" 2>&1 &
GUI_PID=$!
echo "launched isolated GUI pid=$GUI_PID (HOME=$WORK, ports $API_PORT/$TLS_PORT)"

# 4. Wait for the API + first tab.
TOKEN=""
for _ in $(seq 1 30); do
  sleep 0.5
  TOKEN="$(cat "$WORK/.local/state/tab-atelier/api.token" 2>/dev/null || true)"
  [[ -n "$TOKEN" ]] && break
done
BASE="http://127.0.0.1:$API_PORT"
TAB="$(curl -s -m4 "$BASE/tabs" -H "Authorization: Bearer $TOKEN" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["tabs"][0]["id"])' 2>/dev/null || true)"

echo
echo "  BASE=$BASE"
echo "  TOKEN=$TOKEN"
echo "  TAB=$TAB"
echo
echo "Play in the window now. Drive output e.g.:"
echo "  curl -s -XPOST \"$BASE/tabs/by-id/$TAB/input\" -H \"Authorization: Bearer $TOKEN\" --data-binary \$'/usr/games/piu-piu\\r'"
echo

# 5. Hold for the requested duration (or until interrupted).
if [[ "$SECONDS_TO_RUN" -gt 0 ]]; then
  echo "running for ${SECONDS_TO_RUN}s …"
  sleep "$SECONDS_TO_RUN"
else
  echo "running until Ctrl-C …"
  while kill -0 "$GUI_PID" 2>/dev/null; do sleep 1; done
fi
