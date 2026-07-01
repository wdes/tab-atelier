#!/usr/bin/env bash
# Validate the nftables shape of the DOMAIN-allowlist "pre-resolve" model
# (src/net_nft.rs::domain_ruleset + src/net_resolver.rs). The daemon resolves
# the allowlisted domains itself and fills @allow_dyn; the tab resolves via the
# host DNS through a SCOPED :53 hole, and connections are gated at the IP layer.
#
# Checks, against a throwaway per-tab table + cgroup (auto-cleanup):
#   [1] dynamic set with `flags timeout` accepts an element with TTL + comment
#       (exactly what net_resolver's pre-resolve loop programs);
#   [2] the scoped DNS hole: the cgroup may reach the ALLOWED nameserver on :53,
#       but :53 to any OTHER host is dropped (no arbitrary DNS/UDP exfil);
#   [3] enforcement: the cgroup reaches an @allow_dyn IP, other IPs are dropped.
#
# Historical note: an earlier design DNAT'd the tab's :53 to a local resolver.
# That can't work — dst-address DNAT of locally-generated UDP isn't re-routed —
# and the mount-ns fix is blocked by the hardened unit, so we pre-resolve. See
# commit history for the investigation.
#
# Run:  sudo bash scripts/validate-nft-dns.sh
set -uo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root: sudo bash $0" >&2; exit 1; }

NFT=/usr/sbin/nft; [ -x "$NFT" ] || NFT=$(command -v nft || echo nft)
CG=/sys/fs/cgroup
[ -f "$CG/cgroup.controllers" ] || { echo "need cgroup v2 at $CG" >&2; exit 1; }

T=tabatelier_dns_test
REL="tabatelier-dns/tab-x"; TESTCG="$CG/$REL"
LEVEL=$(awk -F/ '{print NF}' <<<"$REL")
NS_OK=1.1.1.1        # stands in for the host's configured nameserver (DNS hole)
NS_BAD=9.9.9.9       # any other :53 host — must be dropped
ALLOW_IP=1.0.0.1     # stands in for a pre-resolved allowlist IP (added to @allow_dyn)
DENY_IP=8.8.8.8      # not in the set — must be dropped

cleanup() {
  "$NFT" delete table inet "$T" 2>/dev/null
  [ -d "$TESTCG" ] && { while read -r p; do echo "$p" > "$CG/cgroup.procs" 2>/dev/null; done < "$TESTCG/cgroup.procs" 2>/dev/null; rmdir "$TESTCG" 2>/dev/null; }
  rmdir "$CG/tabatelier-dns" 2>/dev/null
}
trap cleanup EXIT; cleanup
mkdir -p "$TESTCG" || { echo "cgroup mkdir failed" >&2; exit 1; }

# Run a command inside the test cgroup.
in_cg() { sh -c 'echo $$ > "'"$TESTCG"'/cgroup.procs"; exec "$@"' _ "$@"; }
# Counter value ("packets N") for the rule carrying a given comment.
ctr() { "$NFT" -a list table inet "$T" 2>/dev/null | awk -v c="$1" '$0 ~ c {for(i=1;i<=NF;i++) if($i=="packets"){print $(i+1); exit}}'; }

echo "=> [1/3] domain table: dynamic timeout set + scoped DNS hole (mirrors domain_ruleset)"
if ! "$NFT" -f - <<EOF
table inet $T {
  set allow_dyn { type ipv4_addr; flags timeout; }
  set allow_dyn6 { type ipv6_addr; flags timeout; }
  chain confine {
    oifname "lo" accept comment "loopback"
    ct state established,related accept comment "replies"
    ip daddr { $NS_OK } udp dport 53 counter accept comment "dnshole"
    ip daddr { $NS_OK } tcp dport 53 counter accept comment "dnshole"
    ip daddr @allow_dyn accept comment "preresolved"
    ip6 daddr @allow_dyn6 accept comment "preresolved6"
    counter drop comment "denied"
  }
  chain out {
    type filter hook output priority 0; policy accept;
    socket cgroupv2 level $LEVEL "$REL" jump confine comment "egress allowlist"
  }
}
EOF
then echo "!! nft rejected the domain ruleset (see above)"; exit 2; fi
# The pre-resolve loop programs elements exactly like this:
"$NFT" add element inet "$T" allow_dyn "{ $ALLOW_IP timeout 90s comment \"api.example.com\" }" \
  && echo "   element add ($ALLOW_IP timeout 90s comment) OK" || echo "   !! element add FAILED"

echo "=> [2/3] scoped DNS hole: :53 to the nameserver allowed, :53 elsewhere dropped"
d0=$(ctr denied)
in_cg sh -c "dig +time=2 +tries=1 @$NS_OK example.com >/dev/null 2>&1 || nslookup -timeout=2 example.com $NS_OK >/dev/null 2>&1 || true"
in_cg sh -c "dig +time=2 +tries=1 @$NS_BAD example.com >/dev/null 2>&1 || nslookup -timeout=2 example.com $NS_BAD >/dev/null 2>&1 || true"
sleep 1
hole=$(ctr dnshole); d1=$(ctr denied)
printf "   :53 -> %s (nameserver)  hole-accept counter = %s  %s\n" "$NS_OK" "${hole:-0}" "$([ "${hole:-0}" -gt 0 ] && echo OK || echo '!! not accepted')"
printf "   :53 -> %s (other)       drop delta          = %s  %s\n" "$NS_BAD" "$(( ${d1:-0} - ${d0:-0} ))" "$([ "$(( ${d1:-0} - ${d0:-0} ))" -gt 0 ] && echo 'blocked (good)' || echo '!! not blocked')"

echo "=> [3/3] enforcement: cgroup reaches an @allow_dyn IP, others dropped"
probe(){ in_cg timeout 5 bash -c "exec 3<>/dev/tcp/$1/443" 2>/dev/null && echo "REACHED" || echo "blocked/timeout"; }
printf "   cg -> %s:443 (in @allow_dyn) : %s\n" "$ALLOW_IP" "$(probe "$ALLOW_IP")"
printf "   cg -> %s:443 (not in set)   : %s\n" "$DENY_IP" "$(probe "$DENY_IP")"

echo
echo "Expected PASS: [1] element accepted; [2] nameserver :53 accepted + other :53 blocked; [3] $ALLOW_IP REACHED, $DENY_IP blocked."
echo "live table:"; "$NFT" list table inet "$T" | sed 's/^/   /'
