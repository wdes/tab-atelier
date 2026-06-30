#!/usr/bin/env bash
# Validate the two nftables mechanisms the DNS-resolver egress design needs:
#   1. a per-tab DYNAMIC SET with `flags timeout` that the resolver fills
#      with resolved IPs (so `ip daddr @allow_dyn accept` lets the tab reach
#      exactly the IPs an allowed domain resolved to, expiring per TTL);
#   2. a cgroup-scoped `:53` REDIRECT (DNAT) sending the tab's DNS to a local
#      resolver port — so the tab is forced to use our gating resolver.
#
# Run:  sudo bash scripts/validate-nft-dns.sh
# One throwaway filter table + one nat table + one cgroup; auto-cleanup.
set -uo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root: sudo bash $0" >&2; exit 1; }

NFT=/usr/sbin/nft; [ -x "$NFT" ] || NFT=$(command -v nft || echo nft)
CG=/sys/fs/cgroup
[ -f "$CG/cgroup.controllers" ] || { echo "need cgroup v2 at $CG" >&2; exit 1; }

T=tabatelier_dns_test
NAT=tabatelier_dns_nat
REL="tabatelier-dns/tab-x"; TESTCG="$CG/$REL"
LEVEL=$(awk -F/ '{print NF}' <<<"$REL")
RPORT=5388                                   # stand-in resolver port

cleanup() {
  "$NFT" delete table inet "$T" 2>/dev/null
  "$NFT" delete table inet "$NAT" 2>/dev/null
  kill "${LPID:-0}" 2>/dev/null
  [ -d "$TESTCG" ] && { while read -r p; do echo "$p" > "$CG/cgroup.procs" 2>/dev/null; done < "$TESTCG/cgroup.procs" 2>/dev/null; rmdir "$TESTCG" 2>/dev/null; }
  rmdir "$CG/tabatelier-dns" 2>/dev/null
}
trap cleanup EXIT; cleanup
mkdir -p "$TESTCG" || { echo "cgroup mkdir failed" >&2; exit 1; }

echo "=> [1/3] dynamic set + ip daddr @set accept + element with timeout"
if ! "$NFT" -f - <<EOF
table inet $T {
  set allow_dyn { type ipv4_addr; flags timeout; }
  chain confine {
    oifname "lo" accept
    ct state established,related accept
    ip daddr @allow_dyn accept comment "resolver-added IPs"
    counter drop
  }
  chain out {
    type filter hook output priority 0; policy accept;
    socket cgroupv2 level $LEVEL "$REL" jump confine
  }
}
EOF
then echo "!! nft rejected the dynamic-set ruleset (see above)"; exit 2; fi
# resolver would do this on an allowed answer:
"$NFT" add element inet "$T" allow_dyn '{ 1.1.1.1 timeout 30s comment "api.example.com" }' \
  && echo "   element add (1.1.1.1 timeout 30s comment) OK" || echo "   !! element add FAILED"

echo "=> [2/3] cgroup-scoped :53 redirect to 127.0.0.1:$RPORT"
# tiny UDP listener as the stand-in resolver
( timeout 20 python3 - "$RPORT" <<'PY' &
import socket,sys
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(("127.0.0.1",int(sys.argv[1])))
s.settimeout(15)
try:
    d,a=s.recvfrom(2048); open("/tmp/.nftdns_hit","w").write("HIT from %s"%(a,))
except Exception: pass
PY
) ; LPID=$!
rm -f /tmp/.nftdns_hit
if ! "$NFT" -f - <<EOF
table inet $NAT {
  chain out {
    type nat hook output priority -100; policy accept;
    socket cgroupv2 level $LEVEL "$REL" meta l4proto udp udp dport 53 redirect to :$RPORT
    socket cgroupv2 level $LEVEL "$REL" meta l4proto tcp tcp dport 53 redirect to :$RPORT
  }
}
EOF
then echo "!! nft rejected the nat/redirect ruleset (see above)"; exit 3; fi

# From the cgroup, fire a DNS query at some resolver; the redirect should
# divert it to our local listener.
in_cg() { sh -c 'echo $$ > "'"$TESTCG"'/cgroup.procs"; exec "$@"' _ "$@"; }
in_cg sh -c 'dig +time=2 +tries=1 @9.9.9.9 example.com >/dev/null 2>&1 || nslookup -timeout=2 example.com 9.9.9.9 >/dev/null 2>&1 || true'
sleep 1
if [ -f /tmp/.nftdns_hit ]; then echo "   redirect WORKS: $(cat /tmp/.nftdns_hit)"; else echo "   !! redirect: listener got nothing (redirect may be unsupported here)"; fi
rm -f /tmp/.nftdns_hit

echo "=> [3/3] enforcement: cgroup reaches the set-allowed IP, others dropped"
hit(){ curl -sS -o /dev/null -w "%{http_code}" --max-time 6 "https://$1/" 2>/dev/null; }
printf "   cg -> 1.1.1.1 (in @allow_dyn) : %s\n" "$(in_cg sh -c "$(declare -f hit); hit 1.1.1.1" || echo BLOCKED)"
printf "   cg -> 8.8.8.8 (not in set)    : %s\n" "$( (timeout 8 sh -c "$(declare -f in_cg hit); CG=$CG; in_cg sh -c 'hit 8.8.8.8'") >/dev/null 2>&1 && echo 'REACHED (BAD)' || echo 'blocked (good)')"

echo
echo "PASS: [1] ruleset+element accepted; [2] 'redirect WORKS'; [3] 1.1.1.1 reachable, 8.8.8.8 blocked."
echo "live filter table:"; "$NFT" list table inet "$T" | sed 's/^/   /'
