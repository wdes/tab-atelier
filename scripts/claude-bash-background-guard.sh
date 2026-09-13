#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# PreToolUse guard for Bash: long or waiting work must not run in the foreground.
#
# WHY. A foreground command that waits spends a whole model turn per tick while
# nothing has changed: a 15 minute pipeline polled every 30s costs ~30 turns and
# their tokens, and buys nothing the shell could not have bought on its own.
# Handed to a background command (Bash run_in_background=true) the waiting is
# done by the process, the agent is re-invoked once when it exits, and the
# intermediate ticks cost zero.
#
# The tell is `sleep`, which is what a hand-rolled poll loop is made of. It has
# no other reason to appear in a command here.
#
# Blocks (exit 2, reason on stderr, fed back to the agent) unless the call sets
# run_in_background=true:
#   1. `sleep` at any command position -- the tick of a poll loop.
#   2. explicit watchers: `gh run watch`, `--watch`, `tail -f`, bare `wait`.
#   3. the multi-minute builds: cargo build/test/clippy/deb/apk2, bun/npm build.
#
# Run the waiter in the background instead:
#   scripts/ci-watch.sh            -- the newest commit's CI, one line per change
#   scripts/ci-watch.sh --sha X    -- a specific commit
#
# exit 0 allows. Anything this cannot parse is allowed: a guard that fails
# closed on a payload it does not understand would block correct work.
set -uo pipefail

payload="$(cat)"
tool="$(printf '%s' "$payload" | jq -r '.tool_name // empty' 2>/dev/null)" || exit 0
[ "$tool" = "Bash" ] || exit 0
cmd="$(printf '%s' "$payload" | jq -r '.tool_input.command // empty' 2>/dev/null)" || exit 0
[ -n "$cmd" ] || exit 0

# Already backgrounded: nothing left to ask for.
background="$(printf '%s' "$payload" | jq -r '.tool_input.run_in_background // false' 2>/dev/null)"
[ "$background" = "true" ] && exit 0

block() { printf '%s\n' "$1" >&2; exit 2; }

# A heredoc body is data, not commands: a commit message that happens to quote a
# blocked command is not that command. Scan up to and including the line that
# opens the heredoc, then stop. A payload with no heredoc is scanned whole.
scan="$(printf '%s\n' "$cmd" | awk '{print} /<</{exit}')"

# 0. The wrappers in scripts/ are the sanctioned way to run the heavy commands
#    the rules below block, so a bare call to one is allowed by path. They exist
#    so nobody has to remember the toolchain pin, the two clippy invocations, or
#    which of the waits are safe: the script encodes it and is reviewed as code.
#
#    Only as a sole command. A composite (`scripts/gate.sh && cargo build`) is
#    not vouched for by the wrapper -- it falls through and is judged by the
#    rules below like anything else, which is what catches the cargo build in
#    that example.
#
#    This hook guards a habit, not a boundary: it knows the commands that block
#    in the foreground and allows everything else, so a command it does not
#    recognise is allowed. `rm -rf /`, to pick the obvious case, is none of its
#    business and it will not stop it.
if printf '%s' "$scan" | grep -qE '^[[:space:]]*((bash|sh)[[:space:]]+)?(\./)?([^[:space:]]*/)?scripts/[A-Za-z0-9._-]+\.sh([[:space:]]|$)'; then
  # A bare wrapper call is a path, spaces, and flags. Anything outside that
  # character set means the command is doing something else as well, so it goes
  # through the rules below instead of being waved through.
  if ! printf '%s' "$scan" | grep -qE '[^A-Za-z0-9 ./_=-]'; then
    exit 0
  fi
fi

# 1. sleep -- the tick of a poll loop.
#
#    Matched at any command position rather than only at the start, so
#    `git fetch; sleep 30; gh run list` cannot slip past. The leading boundary
#    excludes word characters and `-`, so `sleep` inside a path or a longer
#    identifier (`--nosleep`) is not a hit.
if printf '%s' "$scan" | grep -qE '(^|[^[:alnum:]_-])sleep[[:space:]]'; then
  block "Blocked: don't \`sleep\` in the foreground. A poll loop wakes you on a timer and spends a model turn per tick while nothing has changed. Re-run this with run_in_background=true and let the shell do the waiting: \`scripts/ci-watch.sh\` for CI (--sha to pick a commit, --once for a single snapshot), or any command plus \`--timeout\` for the rest."
fi

# 2. explicit watchers: their whole purpose is to block until something moves.
if printf '%s' "$scan" | grep -qE 'gh[[:space:]]+run[[:space:]]+watch|--watch|tail[[:space:]]+-[A-Za-z]*f|(^|[;&|(]|&&|\|\|)[[:space:]]*wait([[:space:]]|$|;)'; then
  block "Blocked: a watcher blocks the foreground until something moves, which costs a model turn per tick. Re-run this with run_in_background=true, or use \`scripts/ci-watch.sh\` which prints one line per state change and exits the moment the run settles."
fi

# 3. the multi-minute builds.
#
#    Keyed on a command position so a bare mention ("the fix is in cargo build")
#    does not trip it, and on the subcommand so `cargo fmt` -- seconds, and the
#    pre-commit gate -- stays in the foreground where its output is wanted.
#
#    The `+toolchain` argument is part of the match: this repo cannot build on
#    the system cargo, so every real invocation is `cargo +1.95.0 <sub>` and a
#    pattern that missed it would miss every command it exists to catch.
if printf '%s' "$scan" | grep -qE '(^|[;&|(]|&&|\|\|)[[:space:]]*(cargo[[:space:]]+(\+[^[:space:]]+[[:space:]]+)?|bun[[:space:]]+|npm[[:space:]]+|pnpm[[:space:]]+|yarn[[:space:]]+)(run[[:space:]]+)?(build|test|clippy|deb|apk2|check|install)\b'; then
  block "Blocked: this is a multi-minute build; in the foreground it holds the turn for the whole run. Re-run it with run_in_background=true and read the output when it exits. (cargo fmt stays foreground.)"
fi

exit 0
