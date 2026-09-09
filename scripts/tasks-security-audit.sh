#!/usr/bin/env bash
# @licence MPL-2.0 https://mozilla.org/MPL/2.0/
#
# A backlog source: one security-review task per module that handles input
# from outside the process.
#
#   tab-atelier backlog --from ./scripts/tasks-security-audit.sh
#
# Deliberately NOT every file. A security pass over the pet animation or the
# theme table spends an agent's context to conclude nothing; the modules below
# are the ones that parse a request, resolve a path, spawn a process, hold a
# credential or talk to a network. That is where the bugs that matter live.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

# Ordered roughly by blast radius: anything reachable before authentication
# first, then path/exec handling, then the clients that talk to other hosts.
for f in \
    src/api.rs \
    src/api_ws.rs \
    src/api/files.rs \
    src/api/view.rs \
    src/api/assets.rs \
    src/api/outbox.rs \
    src/api/input.rs \
    src/api/tabs.rs \
    src/api/tokens.rs \
    src/api/claims_route.rs \
    src/api/blackboard_route.rs \
    src/api/fleet_route.rs \
    src/api/net.rs \
    src/api/ssh_agent.rs \
    src/api/limits.rs \
    src/api/catbus.rs \
    src/relay.rs \
    src/remote.rs \
    src/cli/remote/files.rs \
    src/cli/remote/attach.rs \
    src/cli/share_link.rs \
    src/cli/claude_hook.rs \
    src/cli/gossip.rs \
    src/claims.rs \
    src/federation.rs \
    src/briefs.rs \
    src/net_policy.rs \
    src/net_nft.rs \
    src/net_resolver.rs \
    src/ssh_agent.rs \
    src/cgroup.rs \
    src/schedule.rs \
    src/sweep.rs \
    crates/catbus-agent/src/auth.rs \
    crates/catbus-agent/src/session.rs \
    crates/catbus-agent/src/tools.rs \
    ; do
    [ -f "$f" ] || continue
    lines=$(wc -l < "$f")
    printf '%s\t%s\n' \
        "sec:$f" \
        "security review of $f ($lines lines): what can a share-link holder, a remote peer, or a hostile agent make this do? Look for auth gaps, path escapes, injection, secrets in logs or URLs, TOCTOU, and unbounded input. Report findings only — do not change code."
done
