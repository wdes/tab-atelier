#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Launch `codex` as a tracked tab-atelier agent.
#
# tab-atelier learns an agent's kind, session id and state from the agent
# itself (see docs/agent-support-research.md). Claude Code does this through its
# own hook system; codex has hooks too but they require a persisted hook-trust,
# so the short path is to launch codex through this wrapper, which declares the
# same facts using the already-generic `tab-atelier set-status` channel.
#
#   codex-agent.sh [args passed through to codex…]
#
# What it declares:
#   --kind codex            so resume uses `codex resume <id>`, not a daemon relaunch
#   --session <uuid>        the rollout UUID, discovered by matching the cwd
#   --state thinking|idle   coarse, driven by how codex exits
#
# The session id is read from codex's own session index, which carries an
# agent-written `thread_name` — the natural one-line summary for the tab label.
# Falls back to the most recent rollout whose `session_meta.cwd` matches `$PWD`.

set -uo pipefail

CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
TA="${TAB_ATELIER_BIN:-tab-atelier}"
LABEL_MAX=60

# Say something to the daemon when we can; never let telemetry break the agent.
status() {
  "$TA" set-status --kind codex "$@" >/dev/null 2>&1 || true
}

# Newest rollout for this working directory: the one whose session_meta.cwd is
# exactly $PWD. Reading only the header line keeps this cheap.
session_for_cwd() {
  local dir="${CODEX_HOME}/sessions" newest="" f meta
  [[ -d "$dir" ]] || return 1
  while IFS= read -r f; do
    meta=$(head -1 "$f" 2>/dev/null)
    [[ "$meta" == *'"session_meta"'* ]] || continue
    if [[ "$(jq -r '.payload.cwd // empty' <<<"$meta" 2>/dev/null)" == "$PWD" ]]; then
      newest="$f"
      break
    fi
  done < <(find "$dir" -name 'rollout-*.jsonl' -printf '%T@ %p\n' 2>/dev/null | sort -rn | cut -d' ' -f2-)
  [[ -n "$newest" ]] || return 1
  # The UUID in the filename IS the session id (verified against session_meta,
  # see docs/agent-support-research.md). Anchor the strip on the full ISO stamp:
  # a `[0-9T-]+` class is greedy and eats the UUID's own first segment whenever
  # that segment happens to be all digits, e.g. rollout-…-1234-5678-… would be
  # cut down to "5678-…" and `codex resume` would then fail.
  basename "$newest" \
    | sed -E 's/^rollout-[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}-//; s/\.jsonl$//'
}

# Agent-written one-liner for a session id, from codex's own index. Empty when
# the index has no entry yet (a brand new session).
label_for_session() {
  local id="$1" idx="${CODEX_HOME}/session_index.jsonl"
  [[ -n "$id" && -f "$idx" ]] || return 0
  jq -r --arg id "$id" 'select(.id==$id) | .thread_name // empty' "$idx" 2>/dev/null \
    | tail -1 | cut -c1-"$LABEL_MAX"
}

# ── launch ───────────────────────────────────────────────────────────────────
# Declare "running" before handing over: from the tab's point of view the agent
# is thinking as soon as it is launched.
status --state thinking --label "codex (starting)"

codex "$@"
rc=$?

# On exit, refresh the facts now that codex has written its rollout. A session
# that finished cleanly is idle — the indicator goes down but the session stays
# attached and resumable; a non-zero exit is an error worth showing.
sid=$(session_for_cwd || true)
label=$(label_for_session "$sid")
[[ -n "$label" ]] || label="codex (finished)"

# `--opt=value` throughout: codex's own thread_name is arbitrary text and would
# otherwise be taken for a flag if it happened to start with a dash.
if (( rc == 0 )); then
  args=(--state=idle "--label=$label")
else
  args=(--state=error "--label=$label (code $rc)")
fi
[[ -n "$sid" ]] && args+=("--session=$sid")
status "${args[@]}"
exit "$rc"
