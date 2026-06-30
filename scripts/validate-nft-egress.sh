#!/usr/bin/env bash
# Validate the per-tab nftables egress allowlist mechanism that
# `src/net_nft.rs` implements — specifically the `socket cgroupv2` cgroup
# match, which is the one piece that can't be tested unprivileged.
#
# Run:  sudo bash validate-nft-egress.sh
#
# It creates ONE throwaway nft table + ONE test cgroup, runs curl/ping from
# inside the cgroup, and cleans everything up on exit. It does NOT touch any
# existing firewall rules (separate table, OUTPUT policy stays `accept`).
set -uo pipefail

if [ "$(id -u)" != 0 ]; then
  echo "must run as root:  sudo bash $0" >&2
  exit 1
fi

NFT=/usr/sbin/nft
[ -x "$NFT" ] || NFT=$(command -v nft || echo nft)
CG=/sys/fs/cgroup
[ -f "$CG/cgroup.controllers" ] || { echo "need cgroup v2 mounted at $CG" >&2; exit 1; }

TABLE=tabatelier_validate
REL="tabatelier-validate/tab-probe"          # cgroup path relative to the v2 mount
TESTCG="$CG/$REL"
LEVEL=$(awk -F/ '{print NF}' <<<"$REL")        # component count = nft `level`
ALLOW=1.1.1.1                                  # allowlisted (Cloudflare, serves HTTPS)
DENY=8.8.8.8                                   # NOT allowlisted (Google, also serves HTTPS)

cleanup() {
  "$NFT" delete table inet "$TABLE" 2>/dev/null
  if [ -d "$TESTCG" ]; then
    # drain any lingering pids back to root, then remove the dirs
    while read -r p; do echo "$p" > "$CG/cgroup.procs" 2>/dev/null; done < "$TESTCG/cgroup.procs" 2>/dev/null
    rmdir "$TESTCG" 2>/dev/null
    rmdir "$(dirname "$TESTCG")" 2>/dev/null
  fi
}
trap cleanup EXIT
cleanup   # clear any leftovers from a previous run

mkdir -p "$TESTCG" || { echo "could not create test cgroup $TESTCG" >&2; exit 1; }

echo "=> applying nft ruleset (table inet $TABLE, cgroup level $LEVEL \"$REL\")"
if ! "$NFT" -f - <<EOF
table inet $TABLE {
  chain confine {
    oifname "lo" accept comment "loopback (local API, resolver)"
    ct state established,related accept comment "replies"
    udp dport 53 accept comment "dns"
    tcp dport 53 accept comment "dns"
    ip daddr { $ALLOW/32 } accept comment "allowlist v4"
    drop comment "tab-atelier: off-allowlist egress denied"
  }
  chain out {
    type filter hook output priority 0; policy accept;
    socket cgroupv2 level $LEVEL "$REL" jump confine comment "tab-atelier egress allowlist"
  }
}
EOF
then
  echo "!! nft rejected the ruleset (does this nft support 'socket cgroupv2'?). Output:" >&2
  "$NFT" -c -f - <<EOF 2>&1 | sed 's/^/   /'
table inet $TABLE { chain out { type filter hook output priority 0; socket cgroupv2 level $LEVEL "$REL" accept; } }
EOF
  exit 2
fi

# Run a command inside the test cgroup (the sh writes its own pid in first).
in_cg() { sh -c 'echo $$ > "'"$TESTCG"'/cgroup.procs"; exec "$@"' _ "$@"; }
hit()  { curl -sS -o /dev/null -w "%{http_code}" --max-time 6 "https://$1/" 2>/dev/null; }

echo
echo "== host (NOT confined) — both should reach =="
printf "   host -> ALLOW %s : %s\n" "$ALLOW" "$(hit "$ALLOW" || echo FAIL)"
printf "   host -> DENY  %s : %s\n" "$DENY"  "$(hit "$DENY"  || echo FAIL)"

echo
echo "== inside cgroup (egress-confined) — ALLOW reaches, DENY blocked =="
a=$(in_cg sh -c "$(declare -f hit); hit $ALLOW" || true)
printf "   cg -> ALLOW %s : %s\n" "$ALLOW" "${a:-BLOCKED}"
if d=$(timeout 8 sh -c "$(declare -f in_cg hit); export TESTCG='$TESTCG'; in_cg sh -c 'hit $DENY'" 2>/dev/null) && [ -n "$d" ]; then
  printf "   cg -> DENY  %s : %s  <-- BAD: should be blocked\n" "$DENY" "$d"
else
  printf "   cg -> DENY  %s : blocked (good)\n" "$DENY"
fi
printf "   cg -> ping ALLOW : %s\n" "$(in_cg ping -c1 -W3 "$ALLOW" >/dev/null 2>&1 && echo ok || echo no)"
printf "   cg -> ping DENY  : %s\n" "$(in_cg ping -c1 -W3 "$DENY"  >/dev/null 2>&1 && echo 'REACHED (BAD)' || echo 'blocked (good)')"

echo
echo "=> live ruleset:"
"$NFT" list table inet "$TABLE" | sed 's/^/   /'
echo
echo "PASS criteria: host reaches BOTH; cgroup reaches ALLOW only, DENY blocked both curl and ping."
echo "(cleanup is automatic on exit)"
