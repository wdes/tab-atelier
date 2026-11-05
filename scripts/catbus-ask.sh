#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Ask one catbus agent one question, and print its answer.
#
# `--prompt-file -` reads the prompt from stdin, answers it, prints the answer and
# exits: no socket, no wire format, no second language. This used to drive the
# socket directly and needed Python to encode the request, which meant a git hook
# could not ask a question unless the machine had a Python runtime — and a hook
# that depends on one is a hook people disable.
#
# The prompt arrives on stdin because a diff or a commit message can be any size
# and any quoting without touching the shell.
#
#   usage: catbus-ask.sh [timeout-seconds] < prompt
#
# Environment:
#   CATBUS_AGENT          path to the binary (default: this tree's build, then PATH)
#   CATBUS_IDENTITY_FILE  identity for the agent; defaults to the reviewer, so an
#                         agent is never told it is Claude Code when the relay is
#                         not Anthropic

set -u

timeout_s=${1:-180}
here=$(cd "$(dirname "$0")" && pwd)

find_agent() {
	if [ -n "${CATBUS_AGENT:-}" ] && [ -x "$CATBUS_AGENT" ]; then
		printf '%s' "$CATBUS_AGENT"
		return
	fi
	# The workspace build before the installed one: a system binary is whatever the
	# package last shipped, and these identities may use frontmatter only the
	# current source understands. Reaching for the installed one first silently ran
	# an older release and turned a working rewrite into a fallback.
	for p in "$here/../target/release/catbus-agent" "$here/../target/debug/catbus-agent"; do
		[ -x "$p" ] && {
			printf '%s' "$p"
			return
		}
	done
	if command -v catbus-agent >/dev/null 2>&1; then
		printf 'catbus-ask: using the installed catbus-agent, which may predate this tree\n' >&2
		command -v catbus-agent
		return
	fi
	return 1
}

agent=$(find_agent) || {
	cat >&2 <<'EOF'
catbus-ask: no catbus-agent binary found. There is no fallback and no bypass —
the review is the point, so an absent reviewer refuses rather than passes.

    cargo build -p catbus-agent --release

or set CATBUS_AGENT to the binary.
EOF
	exit 1
}

identity=${CATBUS_IDENTITY_FILE:-$here/catbus-reviewer.md}
id_args=''
[ -f "$identity" ] && id_args="--identity-file $identity"

# Nothing to act on and nowhere to ask: the agent is given text and returns text.
# The identity files declare `AllowedTools:` empty; this is the second lock, for
# an identity that forgets to.
tools_config=${CATBUS_REVIEW_TOOLS:-$here/catbus-no-tools.json}
tool_args=''
[ -f "$tools_config" ] && tool_args="--tools-config $tools_config"

# Run from an empty directory: a `.catbus/tools.toml` in the working tree can
# WIDEN a tool set, so an agent that must not act cannot be started inside a repo.
rundir=$(mktemp -d) || exit 1
trap 'rm -rf "$rundir"' EXIT INT TERM

cd "$rundir" || exit 1
# The deadline is the agent's own, not the shell's: a killed process cannot say
# whether it was slow or stuck, and `timeout(1)` is not on every machine the hooks
# run on.
"$agent" --no-tui --once --prompt-file - --ansi=false --timeout "$timeout_s" \
	$id_args $tool_args
