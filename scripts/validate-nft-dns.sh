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
# REDIRECT'ing DNS to a 127.0.0.1 port: the DNAT'd packet keeps its original
# (LAN) source and loops via `lo` to reach the loopback-bound resolver, so it
# needs route_localnet=1 on `lo` (and `all`), and rp_filter must not strictly
# drop it (its reverse path for that source isn't lo). Save/restore each knob.
declare -A SYSCTL_OLD
sysctl_set() { # $1=path $2=value ; remembers the prior value for cleanup
  [ -f "$1" ] || return 0
  SYSCTL_OLD["$1"]=$(cat "$1" 2>/dev/null || echo "")
  echo "$2" > "$1" 2>/dev/null
}
sysctl_restore() { for k in "${!SYSCTL_OLD[@]}"; do echo "${SYSCTL_OLD[$k]}" > "$k" 2>/dev/null; done; }

cleanup() {
  sysctl_restore
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

echo "=> [2/3] cgroup-scoped :53 redirect to 127.0.0.1:$RPORT"
# tiny UDP listener as the stand-in resolver. Background it in THIS shell (no
# `( … & )` subshell) so `$!` is bound to the listener's PID — inside a subshell
# the `&` PID never reaches the parent and `LPID=$!` trips `set -u`.
timeout 20 python3 - "$RPORT" <<'PY' &
import socket,sys
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(("127.0.0.1",int(sys.argv[1])))
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
    socket cgroupv2 level $LEVEL "$REL" meta l4proto udp udp dport 53 counter redirect to :$RPORT
    socket cgroupv2 level $LEVEL "$REL" meta l4proto tcp tcp dport 53 counter redirect to :$RPORT
  }
}
EOF
then echo "!! nft rejected the nat/redirect ruleset (see above)"; exit 3; fi

# Without these the REDIRECT->127.0.0.1:$RPORT is silently dropped (see note at
# the top). The real daemon must do the same when it installs apply_redirect.
sysctl_set /proc/sys/net/ipv4/conf/all/route_localnet 1
sysctl_set /proc/sys/net/ipv4/conf/lo/route_localnet 1
sysctl_set /proc/sys/net/ipv4/conf/all/rp_filter 0
sysctl_set /proc/sys/net/ipv4/conf/lo/rp_filter 0

# From the cgroup, fire a DNS query at some resolver; the redirect should
# divert it to our local listener.
in_cg() { sh -c 'echo $$ > "'"$TESTCG"'/cgroup.procs"; exec "$@"' _ "$@"; }
# Trace where the redirected packet actually goes: lo means it looped back to
# the resolver; the LAN iface means the re-route after DNAT didn't happen.
: > /tmp/.nftdns_pcap
if command -v tcpdump >/dev/null; then
  timeout 4 tcpdump -ni lo -c 6 "port $RPORT or port 53" > /tmp/.nftdns_pcap 2>&1 &
  sleep 0.5
fi
command -v nstat >/dev/null && nstat -n 2>/dev/null   # baseline IP/martian counters
in_cg sh -c 'dig +time=2 +tries=1 @9.9.9.9 example.com >/dev/null 2>&1 || nslookup -timeout=2 example.com 9.9.9.9 >/dev/null 2>&1 || true'
sleep 2
if [ -f /tmp/.nftdns_hit ]; then echo "   redirect WORKS: $(cat /tmp/.nftdns_hit)"; else echo "   !! redirect: listener got nothing"; fi
rm -f /tmp/.nftdns_hit
NSTAT_OUT=$(command -v nstat >/dev/null && nstat 2>/dev/null | grep -iE 'martian|noroute|addrerr|inhdrerr|indiscard|InDiscards|deliver' || echo "")
# Diagnostics to localize a redirect miss: did the NAT rule match at all?
echo "   -- redirect diagnostics --"
echo "   route_localnet all/lo = $(cat /proc/sys/net/ipv4/conf/all/route_localnet 2>/dev/null)/$(cat /proc/sys/net/ipv4/conf/lo/route_localnet 2>/dev/null); rp_filter all/lo = $(cat /proc/sys/net/ipv4/conf/all/rp_filter 2>/dev/null)/$(cat /proc/sys/net/ipv4/conf/lo/rp_filter 2>/dev/null)"
echo "   listener bound: $(ss -lunp 2>/dev/null | grep ":$RPORT " || echo "NOT LISTENING")"
echo "   nat rule counters (nonzero packets => rule matched, delivery is the problem):"
"$NFT" list table inet "$NAT" 2>/dev/null | grep -E "counter|redirect" | sed 's/^/     /'
echo "   tcpdump lo (blank => redirected pkt never reached loopback):"
grep -vE "listening on|link-type|packets (captured|received|dropped)|verbose output" /tmp/.nftdns_pcap 2>/dev/null | sed 's/^/     /' | head -8
rm -f /tmp/.nftdns_pcap
echo "   nstat drops during the query (the kernel's reason for eating it):"
echo "${NSTAT_OUT:-     (none / nstat unavailable)}" | sed 's/^/     /'

echo "=> [3/3] enforcement: cgroup reaches the set-allowed IP, others dropped"
# Raw TCP connect, no TLS: an ALLOWED dest completes the handshake fast, a
# DROPPED dest gets no SYN-ACK and hits the timeout. (The old https-to-bare-IP
# probe returned curl 000 for BOTH cases — a cert-name mismatch on the allowed
# path is indistinguishable from a drop — so it proved nothing.)
probe(){ in_cg timeout 5 bash -c "exec 3<>/dev/tcp/$1/443" 2>/dev/null && echo "REACHED" || echo "blocked/timeout"; }
printf "   cg -> 1.1.1.1:443 (in @allow_dyn) : %s\n" "$(probe 1.1.1.1)"
printf "   cg -> 8.8.8.8:443 (not in set)    : %s\n" "$(probe 8.8.8.8)"

echo
echo "Expected PASS: [1] ruleset+element accepted; [2] 'redirect WORKS'; [3] 1.1.1.1:443 REACHED, 8.8.8.8:443 blocked."
echo "live filter table:"; "$NFT" list table inet "$T" | sed 's/^/   /'
