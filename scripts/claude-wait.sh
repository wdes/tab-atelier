#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# claude-wait.sh — wait for a condition without holding a model turn.
#
# A pause in the foreground spends a turn per tick and reports nothing new. Hand
# the wait to this script as a background command: the shell does the waiting,
# and you are re-invoked once, when it is over.
#
#   scripts/claude-wait.sh --pid 1234            # until process 1234 exits
#   scripts/claude-wait.sh --file /tmp/done      # until the path exists
#   scripts/claude-wait.sh --gone /tmp/lock      # until the path is gone
#   scripts/claude-wait.sh --port 7890           # until the port accepts
#   scripts/claude-wait.sh --http https://x/     # until HTTP 2xx
#
#   --every N     seconds between checks (default 10)
#   --timeout N   give up after N seconds (default 600)
#
# Exit: 0 the condition held, 1 it timed out, 2 bad usage.
set -uo pipefail

every=10
timeout=600
kind=''
target=''

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) kind="pid"; target="${2?--pid needs a value}"; shift 2 ;;
    --file) kind="file"; target="${2?--file needs a value}"; shift 2 ;;
    --gone) kind="gone"; target="${2?--gone needs a value}"; shift 2 ;;
    --port) kind="port"; target="${2?--port needs a value}"; shift 2 ;;
    --http) kind="http"; target="${2?--http needs a value}"; shift 2 ;;
    --every) every="${2?--every needs a value}"; shift 2 ;;
    --timeout) timeout="${2?--timeout needs a value}"; shift 2 ;;
    -h | --help)
      sed -n '3,/^set /p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'claude-wait: unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if [ -z "$kind" ]; then
  printf 'claude-wait: give one of --pid --file --gone --port --http\n' >&2
  exit 2
fi

held() {
  case "$kind" in
    pid) ! kill -0 "$target" 2>/dev/null ;;
    file) [ -e "$target" ] ;;
    gone) [ ! -e "$target" ] ;;
    # bash's /dev/tcp: a connect that succeeds means something is listening.
    port) (exec 3<>"/dev/tcp/127.0.0.1/$target") 2>/dev/null ;;
    http)
      command -v browserget >/dev/null 2>&1 || return 1
      browserget --json --timeout "$every" "$target" 2>/dev/null |
        jq -e '.status >= 200 and .status < 300' >/dev/null 2>&1
      ;;
  esac
}

started="$(date +%s)"
while :; do
  if held; then
    printf 'claude-wait: %s %s ready after %ss\n' "$kind" "$target" "$(($(date +%s) - started))"
    exit 0
  fi
  if [ "$(($(date +%s) - started))" -ge "$timeout" ]; then
    printf 'claude-wait: %s %s not ready after %ss\n' "$kind" "$target" "$timeout" >&2
    exit 1
  fi
  sleep "$every"
done
