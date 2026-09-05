#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# End-to-end exercise of the self-organising fleet, against real daemons.
#
# Two headless instances run in isolated HOME/XDG sandboxes on spare ports, so
# nothing here touches the developer's own tabs, state or blackboard. No agents
# are launched: the agents are shell loops, which is the honest way to test the
# protocol without spending tokens or depending on a model's behaviour.
#
# What it asserts, in order:
#   1. backlog turns a coverage report into announced work
#   2. concurrent takes never hand one task to two agents   <- the whole point
#   3. done closes the task and frees the lease
#   4. gossip converges two hosts that never coordinated
#   5. a second backlog sweep announces nothing (idempotent)
#
# Usage: scripts/self-org-sandbox.sh [--keep]
set -uo pipefail

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/debug/tab-atelier-headless"
SBOX="${TMPDIR:-/tmp}/ta-selforg.$$"
PORT_A=7981
PORT_B=7982

pass=0
fail=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
no() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1 (want '$3', got '$2')"; fi; }

cleanup() {
    for port in "$PORT_A" "$PORT_B"; do
        pid=$(ss -ltnp 2>/dev/null | awk -v p=":$port" '$0 ~ p {match($0, /pid=([0-9]+)/, m); print m[1]}')
        [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null
    done
    sleep 0.5
    [ "$KEEP" = "0" ] && rm -rf "$SBOX"
    [ "$KEEP" = "1" ] && echo "sandbox kept at $SBOX"
    return 0
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "build first: cargo build --no-default-features --features headless"; exit 1; }

# --- two isolated hosts ------------------------------------------------------
# `env -i` so nothing from the caller's environment leaks in: the daemons must
# find their own state, not the developer's.
host_env() {
    local root="$SBOX/$1" port="$2"
    echo "env -i PATH=/usr/bin:/bin HOME=$root/home XDG_STATE_HOME=$root/state" \
        "XDG_CONFIG_HOME=$root/config XDG_CACHE_HOME=$root/cache TAB_ATELIER_API_PORT=$port"
}

for h in a:$PORT_A b:$PORT_B; do
    name="${h%%:*}"; port="${h##*:}"
    mkdir -p "$SBOX/$name"/{home,state,config/tab-atelier,cache}
    printf '{"api_addr":"127.0.0.1:%s"}\n' "$port" > "$SBOX/$name/config/tab-atelier/preferences.json"
    # shellcheck disable=SC2086
    (cd "$SBOX/$name" && $(host_env "$name" "$port") nohup "$BIN" > "$SBOX/$name/daemon.log" 2>&1 &)
done
sleep 3

A="$(host_env a $PORT_A) $BIN"
B="$(host_env b $PORT_B) $BIN"

echo "== hosts =="
for h in a:$PORT_A b:$PORT_B; do
    name="${h%%:*}"; port="${h##*:}"
    if ss -ltn 2>/dev/null | grep -q "127.0.0.1:$port"; then ok "host $name listening on $port"; else
        no "host $name did not start"; sed -n '1,5p' "$SBOX/$name/daemon.log"; exit 1
    fi
done

# --- 1. the backlog generates work from measured state -----------------------
echo "== 1. backlog: coverage report -> announced work =="
cat > "$SBOX/lcov.info" <<'LCOV'
SF:src/api.rs
LF:1000
LH:400
end_of_record
SF:src/relay.rs
LF:600
LH:300
end_of_record
SF:src/app.rs
LF:800
LH:200
end_of_record
SF:src/tiny.rs
LF:8
LH:0
end_of_record
SF:src/done.rs
LF:300
LH:290
end_of_record
LCOV

# shellcheck disable=SC2086
out=$($A backlog --lcov "$SBOX/lcov.info" --limit 3 --audit-largest 1 2>&1)
announced=$(echo "$out" | grep -c '^announced ')
check "announced 3 coverage tasks + 1 audit" "$announced" "4"
echo "$out" | sed 's/^/     /'
# shellcheck disable=SC2086
$A tasks | sed 's/^/     /'
# src/tiny.rs is 0% covered but only 8 lines — too small to be worth a task.
# shellcheck disable=SC2086
if $A tasks | grep -q "tiny.rs"; then no "announced a file below --min-lines"; else ok "skipped the file too small to be worth an agent"; fi
# src/done.rs is already above target.
# shellcheck disable=SC2086
if $A tasks | grep -q "cov:src/done.rs"; then no "announced an already-covered file"; else ok "skipped the file already above target"; fi

# --- 2. concurrent takes: the mutual-exclusion property ----------------------
echo "== 2. six agents take at once =="
# The real test. Every agent sees the same board and reaches for work with no
# coordination; the lease is the only thing preventing two from taking one task.
rm -f "$SBOX/taken."*
for i in 1 2 3 4 5 6; do
    # shellcheck disable=SC2086
    ( $(host_env a $PORT_A) TAB_ATELIER_AGENT="agent-$i" "$BIN" take --ttl 300 \
        > "$SBOX/taken.$i" 2>&1 ) &
done
wait

grep -h '^\[' "$SBOX/taken."* 2>/dev/null | sed 's/^/     /'
taken=$(grep -h '^\[' "$SBOX/taken."* 2>/dev/null | sed 's/^\[\([^]]*\)\].*/\1/' | sort)
n_taken=$(echo "$taken" | grep -c . )
n_uniq=$(echo "$taken" | sort -u | grep -c . )
check "every taken task is distinct (no two agents on one task)" "$n_taken" "$n_uniq"
check "all four tasks were taken" "$n_uniq" "4"
# Two agents found nothing left, which is correct, not an error.
idle=$(grep -l "nothing free\|nothing open" "$SBOX/taken."* 2>/dev/null | wc -l)
check "the two agents with no work left said so" "$idle" "2"

# The daemon's own view must agree with what the agents believe.
leases=$(curl -s -H "Authorization: Bearer $($(host_env a $PORT_A) "$BIN" token)" \
    "http://127.0.0.1:$PORT_A/claims" 2>/dev/null | grep -o '"key"' | wc -l)
check "the daemon holds one lease per taken task" "$leases" "4"

# --- 3. done closes the task and frees the lease -----------------------------
echo "== 3. done =="
first_task=$(echo "$taken" | head -1)
first_agent=$(grep -l "\[$first_task\]" "$SBOX/taken."* | head -1 | sed 's/.*taken\.//')
# shellcheck disable=SC2086
# `done` quoted: it is a shell keyword, and an unquoted one here reads badly
# even though bash accepts it in argument position.
$(host_env a $PORT_A) TAB_ATELIER_AGENT="agent-$first_agent" "$BIN" "done" "$first_task" "coverage 40% -> 82%" > /dev/null
# shellcheck disable=SC2086
if $A tasks | grep -q "$first_task"; then no "a finished task is still listed as open"; else ok "finished work left the open board"; fi
# shellcheck disable=SC2086
if $A tasks --all | grep -q "done .*$first_task\|$first_task"; then ok "--all still shows the finished task"; else no "finished task vanished entirely"; fi
leases_after=$(curl -s -H "Authorization: Bearer $($(host_env a $PORT_A) "$BIN" token)" \
    "http://127.0.0.1:$PORT_A/claims" 2>/dev/null | grep -o '"key"' | wc -l)
check "done released the lease immediately" "$leases_after" "3"

# --- 4. gossip converges two hosts -------------------------------------------
echo "== 4. gossip =="
# shellcheck disable=SC2086
b_token=$($(host_env b $PORT_B) "$BIN" token)
# shellcheck disable=SC2086
$A remote add --label peer-b --url "http://127.0.0.1:$PORT_B" --token "$b_token" > /dev/null 2>&1 \
    || python3 - "$SBOX/a/config/tab-atelier/preferences.json" "$PORT_B" "$b_token" <<'PY'
import json, sys, uuid
path, port, token = sys.argv[1], sys.argv[2], sys.argv[3]
doc = json.load(open(path))
doc.setdefault("remote_endpoints", []).append({
    "id": str(uuid.uuid4()), "label": "peer-b", "url": f"http://127.0.0.1:{port}",
    "token": token, "cert_sha256": "", "autoconnect": False,
})
json.dump(doc, open(path, "w"))
PY

# shellcheck disable=SC2086
before=$($B tasks --all | grep -c '^\[' || true)
check "host b starts with an empty board" "$before" "0"
# shellcheck disable=SC2086
$A gossip | sed 's/^/     /'
# shellcheck disable=SC2086
after=$($B tasks --all | grep -c '^\[' || true)
check "host b now sees every task host a knows" "$after" "4"
# shellcheck disable=SC2086
if $B tasks --all | grep -q "coverage 40% -> 82%"; then ok "the completion result crossed hosts too"; else no "done entry did not converge"; fi

# Idempotent: a second round exchanges nothing new.
# shellcheck disable=SC2086
second=$($A gossip)
echo "$second" | sed 's/^/     /'
if echo "$second" | grep -q "pulled 0 pushed 0"; then ok "a second round is a no-op (union is idempotent)"; else no "gossip kept re-sending: $second"; fi

# The round trip that matters: work announced on A, taken on B, and A finds
# out — one fleet across two machines, with nothing coordinating them.
# shellcheck disable=SC2086
$A announce "cov:src/new.rs" "raise coverage of src/new.rs" > /dev/null
# shellcheck disable=SC2086
$A gossip -q > /dev/null
# shellcheck disable=SC2086
b_take=$($(host_env b $PORT_B) TAB_ATELIER_AGENT="b-agent" "$BIN" take --ttl 60 2>&1)
if echo "$b_take" | grep -q 'cov:src/new.rs'; then ok "host b took work that host a announced"; else no "host b could not take: $b_take"; fi
# shellcheck disable=SC2086
$A gossip | sed 's/^/     /'
# shellcheck disable=SC2086
if $A tasks --all | grep "cov:src/new.rs" | grep -q "b-agent"; then
    ok "host a learned that b-agent took it (bidirectional convergence)"
else
    no "the award did not travel back to host a"
    # shellcheck disable=SC2086
    $A tasks --all | grep "new.rs" | sed 's/^/       /'
fi

# --- 5. the sweep is idempotent ----------------------------------------------
echo "== 5. re-running backlog =="
# shellcheck disable=SC2086
again=$($A backlog --lcov "$SBOX/lcov.info" --limit 3 --audit-largest 1 2>&1)
echo "$again" | sed 's/^/     /'
if echo "$again" | grep -q "nothing to announce"; then ok "a repeat sweep announces nothing"; else no "duplicate tasks: $again"; fi
# The finished one is still cooling, so it does not come back either.
if echo "$again" | grep -q "$first_task"; then no "re-announced a task finished seconds ago"; else ok "finished work stayed off the board (cooldown)"; fi

echo
printf '== %d passed, %d failed ==\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
