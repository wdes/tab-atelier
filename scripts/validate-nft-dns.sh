#!/usr/bin/env bash
# Validate the two nftables mechanisms the DNS-resolver egress design needs:
#   1. a per-tab DYNAMIC SET with `flags timeout` that the resolver fills
#      with resolved IPs (so `ip daddr @allow_dyn accept` lets the tab reach
#      exactly the IPs an allowed domain resolved to, expiring per TTL);
#   2. a cgroup-scoped port remap (`:53` -> resolver port) on a DEDICATED local
#      address the tab targets directly — so the tab's DNS lands on our gating
#      resolver. (Redirecting DNS aimed at an EXTERNAL resolver does NOT work;
#      see the DNS_ADDR note below for why.)
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
# Dedicated NON-loopback local address for the resolver, added to `lo`. This is
# the crux of the working design: a tab must send DNS *directly* to this addr
# (its resolv.conf points here) and nft remaps only the PORT (53 -> resolver).
#
# What does NOT work, and why: redirecting DNS aimed at an EXTERNAL resolver to a
# local port. `redirect to :port` / `dnat to <local>:port` changes the
# destination ADDRESS of a locally-generated UDP packet, but the socket's cached
# route wins and the kernel does not re-route it — the packet egresses toward
# the original (external) route instead of being delivered locally. The nft
# counter increments (rule matched) yet nothing reaches the resolver, with no
# martian/no-route SNMP drop. Keeping the dst address LOCAL (tab targets
# $DNS_ADDR, we remap only the port) means no re-route, so it delivers. No
# route_localnet / rp_filter changes are needed (unlike a 127.x target).
DNS_ADDR=192.0.2.53                          # TEST-NET-1 (RFC5737), non-routable

cleanup() {
  ip addr del "$DNS_ADDR/32" dev lo 2>/dev/null
  "$NFT" delete table inet "$T" 2>/dev/null
  "$NFT" delete table inet "$NAT" 2>/dev/null
  # NB: guard the kill — `kill "${LPID:-0}"` becomes `kill 0` on the pre-flight
  # cleanup (LPID isn't set until [2/3]), and `kill 0` SIGTERMs the whole
  # process group, silently killing this script (exit 143) before it prints a
  # line. Only fires as root, since non-root exits at the id check first.
  [ -n "${LPID:-}" ] && kill "$LPID" 2>/dev/null
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

echo "=> [2/3] tab queries $DNS_ADDR directly; cgroup nft remaps :53 -> :$RPORT"
# Give the resolver a real (non-loopback) local address on lo, so the DNAT'd
# packet is delivered as ordinary local traffic instead of hitting the
# loopback-source martian drop that killed `redirect to :port`.
ip addr add "$DNS_ADDR/32" dev lo 2>/dev/null

# tiny UDP listener as the stand-in resolver, bound on the dedicated addr.
# Background it in THIS shell (no `( … & )` subshell) so `$!` is bound to the
# listener's PID — inside a subshell the `&` PID never reaches the parent.
timeout 20 python3 - "$DNS_ADDR" "$RPORT" <<'PY' &
import socket,sys
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind((sys.argv[1],int(sys.argv[2])))
s.settimeout(15)
try:
    d,a=s.recvfrom(2048); open("/tmp/.nftdns_hit","w").write("HIT from %s"%(a,))
except Exception: pass
PY
LPID=$!
rm -f /tmp/.nftdns_hit
if ! "$NFT" -f - <<EOF
table inet $NAT {
  chain out {
    type nat hook output priority -100; policy accept;
    socket cgroupv2 level $LEVEL "$REL" meta l4proto udp udp dport 53 counter dnat ip to $DNS_ADDR:$RPORT
    socket cgroupv2 level $LEVEL "$REL" meta l4proto tcp tcp dport 53 counter dnat ip to $DNS_ADDR:$RPORT
  }
}
EOF
then echo "!! nft rejected the nat/dnat ruleset (see above)"; exit 3; fi

in_cg() { sh -c 'echo $$ > "'"$TESTCG"'/cgroup.procs"; exec "$@"' _ "$@"; }
# Trace the packet on lo: it should now show $DNS_ADDR.<sport> > $DNS_ADDR:$RPORT.
: > /tmp/.nftdns_pcap
if command -v tcpdump >/dev/null; then
  timeout 4 tcpdump -ni lo -c 6 "port $RPORT or port 53" > /tmp/.nftdns_pcap 2>&1 &
  sleep 0.5
fi
# Query the dedicated addr DIRECTLY (this is what the tab's resolv.conf would
# point at). The dst address is already local, so the DNAT only remaps the port
# (53->resolver) — no destination re-route, which is what killed the earlier
# redirect-from-external attempts.
in_cg sh -c "dig +time=2 +tries=1 @$DNS_ADDR example.com >/dev/null 2>&1 || nslookup -timeout=2 example.com $DNS_ADDR >/dev/null 2>&1 || true"
sleep 2
if [ -f /tmp/.nftdns_hit ]; then echo "   DNS reached the resolver: $(cat /tmp/.nftdns_hit)"; else echo "   !! resolver got nothing"; fi
rm -f /tmp/.nftdns_hit
# Diagnostics (kept for regression debugging if this ever stops delivering).
echo "   -- dnat diagnostics --"
echo "   nat rule counters (nonzero packets => rule matched):"
"$NFT" list table inet "$NAT" 2>/dev/null | grep -E "counter|dnat" | sed 's/^/     /'
echo "   tcpdump lo (should show $DNS_ADDR.<sport> > $DNS_ADDR:$RPORT):"
grep -vE "listening on|link-type|packets (captured|received|dropped)|verbose output" /tmp/.nftdns_pcap 2>/dev/null | sed 's/^/     /' | head -8
rm -f /tmp/.nftdns_pcap

echo "=> [3/3] enforcement: cgroup reaches the set-allowed IP, others dropped"
# Raw TCP connect, no TLS: an ALLOWED dest completes the handshake fast, a
# DROPPED dest gets no SYN-ACK and hits the timeout. (The old https-to-bare-IP
# probe returned curl 000 for BOTH cases — a cert-name mismatch on the allowed
# path is indistinguishable from a drop — so it proved nothing.)
probe(){ in_cg timeout 5 bash -c "exec 3<>/dev/tcp/$1/443" 2>/dev/null && echo "REACHED" || echo "blocked/timeout"; }
printf "   cg -> 1.1.1.1:443 (in @allow_dyn) : %s\n" "$(probe 1.1.1.1)"
printf "   cg -> 8.8.8.8:443 (not in set)    : %s\n" "$(probe 8.8.8.8)"

echo
echo "Expected PASS: [1] ruleset+element accepted; [2] DNS reached the resolver; [3] 1.1.1.1:443 REACHED, 8.8.8.8:443 blocked."
echo "live filter table:"; "$NFT" list table inet "$T" | sed 's/^/   /'
