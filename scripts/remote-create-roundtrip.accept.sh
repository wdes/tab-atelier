#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# REAL acceptance for `tab-atelier remote spawn` (RemoteCommand::Create):
# boots TWO isolated headless daemons A and B, then from A issues an
# INLINE remote-spawn (--url/--token, no prefs) that must create a brand
# new tab on B. Asserts against B's real GET /tabs (Bearer) — a genuine
# A->B round-trip, no mock. Follows ~/Dev/outbox/isolated-test-daemon-recipe.md
# (isolated HOME, distinct ports, api_addr + api_tls_addr set per daemon
# so the :7891 TLS default can't collide, teardown by captured PID).
#
# Exit 0 = the new tab appears on B with the expected name AND cwd.
# RED-before (no `spawn` verb / no Create) → the spawn call fails → exit 1.
set -u

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
BIN="$ROOT/target/debug/tab-atelier-headless"
[ -x "$BIN" ] || { echo "FAIL: build first: cargo build --no-default-features --features headless,energy --bin tab-atelier-headless"; exit 1; }

HOME_A=$(mktemp -d /tmp/ta-A.XXXX); PORT_A=7920; TLS_A=7921
HOME_B=$(mktemp -d /tmp/ta-B.XXXX); PORT_B=7930; TLS_B=7931
SPAWN_CWD=$(mktemp -d /tmp/ta-spawn-cwd.XXXX)
TAB_NAME="from-A-$$"

cleanup() {
    [ -f "$HOME_A/daemon.pid" ] && kill "$(cat "$HOME_A/daemon.pid")" 2>/dev/null
    [ -f "$HOME_B/daemon.pid" ] && kill "$(cat "$HOME_B/daemon.pid")" 2>/dev/null
    rm -rf "$HOME_A" "$HOME_B" "$SPAWN_CWD"
}
trap cleanup EXIT

boot() { # $1=HOME $2=api_port $3=tls_port
    mkdir -p "$1/.config/tab-atelier"
    printf '{"api_addr":"127.0.0.1:%s","api_tls_addr":"127.0.0.1:%s"}\n' "$2" "$3" \
        > "$1/.config/tab-atelier/preferences.json"
    HOME="$1" nohup "$BIN" > "$1/daemon.log" 2>&1 &
    echo $! > "$1/daemon.pid"
}

token_of() { # $1=HOME ; waits for the auto-generated token
    local f="$1/.local/state/tab-atelier/api.token" i=0
    while [ $i -lt 50 ]; do [ -s "$f" ] && { cat "$f"; return 0; }; sleep 0.1; i=$((i+1)); done
    return 1
}

tabs_json() { # $1=port $2=token
    curl -s --max-time 5 -H "Authorization: Bearer $2" "http://127.0.0.1:$1/tabs"
}

echo "== booting daemons A (:$PORT_A) and B (:$PORT_B) =="
boot "$HOME_A" "$PORT_A" "$TLS_A"
boot "$HOME_B" "$PORT_B" "$TLS_B"
TOKEN_A=$(token_of "$HOME_A") || { echo "FAIL: A never wrote its token"; cat "$HOME_A/daemon.log"; exit 1; }
TOKEN_B=$(token_of "$HOME_B") || { echo "FAIL: B never wrote its token"; cat "$HOME_B/daemon.log"; exit 1; }

# Wait until B answers /tabs, then snapshot its baseline tab count.
for i in $(seq 1 50); do tabs_json "$PORT_B" "$TOKEN_B" | grep -q '"tabs"' && break; sleep 0.1; done
BEFORE=$(tabs_json "$PORT_B" "$TOKEN_B" | grep -o '"id"' | wc -l)
echo "B baseline: $BEFORE tab(s)"

echo "== A --inline--> B : remote spawn (name=$TAB_NAME cwd=$SPAWN_CWD) =="
# HOME_A only isolates A's own state; the spawn verb reads NO prefs — the
# endpoint is 100% inline (--url/--token of B).
if ! HOME="$HOME_A" "$BIN" remote spawn \
        --url "http://127.0.0.1:$PORT_B" --token "$TOKEN_B" \
        --path "$SPAWN_CWD" --name "$TAB_NAME"; then
    echo "FAIL: remote spawn returned non-zero"
    exit 1
fi

# Independent assertion against B's real /tabs (not the CLI's own verdict).
AFTER_JSON=$(tabs_json "$PORT_B" "$TOKEN_B")
AFTER=$(printf '%s' "$AFTER_JSON" | grep -o '"id"' | wc -l)
echo "B after:    $AFTER tab(s)"

fail=0
[ "$AFTER" -gt "$BEFORE" ] || { echo "FAIL: tab count did not grow on B ($BEFORE -> $AFTER)"; fail=1; }
printf '%s' "$AFTER_JSON" | grep -Eq "\"name\": *\"$TAB_NAME\"" \
    || { echo "FAIL: no tab named '$TAB_NAME' on B"; fail=1; }
printf '%s' "$AFTER_JSON" | grep -Eq "\"cwd\": *\"$SPAWN_CWD\"" \
    || { echo "FAIL: no tab with cwd '$SPAWN_CWD' on B"; fail=1; }
if [ "$fail" -eq 0 ]; then
    echo "PASS (positive): A->B inline remote spawn created a real tab on B (name+cwd match)"
else
    echo "--- B /tabs dump ---"; printf '%s\n' "$AFTER_JSON"
fi

# ---- Negative: the Bearer gate must actually protect (401 -> zero ghost tab) ----
# A spawn with the WRONG token must be refused by B and leave its tab list
# untouched — the proof the round-trip rides a real authenticated POST, not a
# client-side fabrication. Without this the "aussi protégé que github" claim
# is unverified.
echo "== A --inline--> B : remote spawn with a BOGUS token (must be refused) =="
NEG_BEFORE=$(tabs_json "$PORT_B" "$TOKEN_B" | grep -o '"id"' | wc -l)
if HOME="$HOME_A" "$BIN" remote spawn \
        --url "http://127.0.0.1:$PORT_B" --token "deadbeefdeadbeefdeadbeefdeadbeef" \
        --path "$SPAWN_CWD" --name "ghost-$$"; then
    echo "FAIL (negative): bogus-token spawn returned exit 0 — the Bearer gate did not refuse it"
    fail=1
else
    echo "  (bogus-token spawn correctly returned non-zero)"
fi
NEG_AFTER_JSON=$(tabs_json "$PORT_B" "$TOKEN_B")
NEG_AFTER=$(printf '%s' "$NEG_AFTER_JSON" | grep -o '"id"' | wc -l)
[ "$NEG_AFTER" -eq "$NEG_BEFORE" ] \
    || { echo "FAIL (negative): B tab count changed ($NEG_BEFORE -> $NEG_AFTER) — a ghost tab was created"; fail=1; }
printf '%s' "$NEG_AFTER_JSON" | grep -Eq "\"name\": *\"ghost-$$\"" \
    && { echo "FAIL (negative): a 'ghost-$$' tab exists on B"; fail=1; }
[ "$fail" -eq 0 ] && echo "PASS (negative): bogus token refused (401), B unchanged ($NEG_BEFORE tab(s)), zero ghost tab"

if [ "$fail" -eq 0 ]; then
    echo "PASS: positive round-trip + negative auth gate both green"
    exit 0
fi
echo "--- B /tabs dump (final) ---"; printf '%s\n' "$NEG_AFTER_JSON"
exit 1
