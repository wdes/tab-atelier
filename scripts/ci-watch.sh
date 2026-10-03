#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# ci-watch.sh — wait for CI in the shell, not in the model.
#
# Run it in the background. It ticks every 60s, prints one line only when
# something changes, and exits the moment the run settles — so a 15 minute
# pipeline costs zero model turns instead of one per poll.
#
#   scripts/ci-watch.sh                 # watch the newest commit's runs
#   scripts/ci-watch.sh --sha 20e2543   # watch a specific commit
#   scripts/ci-watch.sh --once          # one snapshot, no waiting
#
# Exit: 0 everything green, 1 something failed, 3 timed out.

set -uo pipefail

INTERVAL=60
TIMEOUT=$((60 * 60))
SHA=""
ONCE=0
REPO_ARGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --interval) INTERVAL="${2:?--interval needs a value}"; shift 2 ;;
    --timeout)  TIMEOUT="${2:?--timeout needs a value}"; shift 2 ;;
    --sha)      SHA="${2:?--sha needs a value}"; shift 2 ;;
    --repo)     REPO_ARGS=(--repo "${2:?--repo needs a value}"); shift 2 ;;
    --once)     ONCE=1; shift ;;
    -h|--help)  sed -n '3,/^set /p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

command -v gh >/dev/null || { echo "gh is not on PATH" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is not on PATH" >&2; exit 2; }

fetch() {
  gh run list --limit 60 --json workflowName,status,conclusion,headSha "${REPO_ARGS[@]}" 2>/dev/null
}

# One compact line: what is still running, and what already broke.
#
# `headSha` is the full 40-char hash and `--sha` is usually a prefix of it, so
# the match is `startswith`, not equality -- an equality test silently reports
# "no runs yet" for every short sha it is handed.
snapshot() {
  jq -r --arg sha "$SHA" '
    [.[] | select(.headSha | startswith($sha))] as $r
    | if ($r | length) == 0 then "\($sha[0:7]) waiting: no runs yet"
      else
        ($r | map(select(.status != "completed"))) as $live
        | ($r | map(select(.conclusion == "failure" or .conclusion == "startup_failure" or .conclusion == "timed_out"))) as $bad
        | ($r | map(select(.conclusion == "success"))) as $ok
        | if ($bad | length) > 0 then
            "\($sha[0:7]) FAILED \($bad | map(.workflowName) | join(", "))"
          elif ($live | length) > 0 then
            "\($sha[0:7]) \($ok | length) ok, running: \($live | map(.workflowName) | join(", "))"
          else
            "\($sha[0:7]) all \($ok | length) green"
          end
      end' <<<"$1"
}

# live bad ok  — "none" while the push has not produced a run yet.
tally() {
  jq -r --arg sha "$SHA" '
    [.[] | select(.headSha | startswith($sha))] as $r
    | if ($r | length) == 0 then "none"
      else
        ($r | map(select(.status != "completed")) | length) as $live
        | ($r | map(select(.conclusion == "failure" or .conclusion == "startup_failure" or .conclusion == "timed_out")) | length) as $bad
        | ($r | map(select(.conclusion == "success")) | length) as $ok
        | "\($live) \($bad) \($ok)"
      end' <<<"$1"
}

json="$(fetch)" || { echo "gh run list failed" >&2; exit 2; }

if [ -z "$SHA" ]; then
  SHA="$(jq -r '.[0].headSha // empty' <<<"$json")"
  [ -n "$SHA" ] || { echo "no CI runs found" >&2; exit 2; }
fi

# --once reports and returns: 1 only if something actually failed. A run still
# in flight is not a failure, which is why this does not reuse the loop's exit.
if [ "$ONCE" = 1 ]; then
  snapshot "$json"
  # "none" has no numbers to read; nothing has failed if nothing has run.
  counts="$(tally "$json")"
  if [ "$counts" != "none" ]; then
    read -r _ bad _ <<<"$counts"
    [ "$bad" -gt 0 ] && exit 1
  fi
  exit 0
fi

start="$(date +%s)"
previous=""

while :; do
  line="$(snapshot "$json")"
  if [ "$line" != "$previous" ]; then
    printf '%s %s\n' "$(date +%H:%M:%S)" "$line"
    previous="$line"
  fi

  read -r live bad ok <<<"$(tally "$json")"

  # Settled only once a run exists and nothing is left in flight.
  if [ "$live" = "0" ] && [ $((bad + ok)) -gt 0 ]; then
    if [ "$bad" -gt 0 ]; then
      echo "---- $SHA: $bad failed, $ok green"
      exit 1
    fi
    echo "---- $SHA: $ok green"
    exit 0
  fi

  elapsed=$(( $(date +%s) - start ))
  if [ "$elapsed" -ge "$TIMEOUT" ]; then
    echo "---- timed out after ${elapsed}s with $live still running"
    exit 3
  fi

  sleep "$INTERVAL"
  json="$(fetch)" || echo "$(date +%H:%M:%S) gh run list failed, retrying" >&2
done
