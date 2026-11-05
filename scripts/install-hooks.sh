#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Wire the repository's hooks, once per clone.
#
# The hooks live in `.githooks/` and git ignores them until `core.hooksPath`
# points there. Without this, a fresh clone commits, pushes, and publishes
# whatever the author typed — including whatever they pasted out of a bug report.
#
# There is no bypass in the hooks themselves. `git commit --no-verify` and
# `git push --no-verify` still skip them, which is why CI re-runs the message scan
# over the pushed range; the CI half cannot be declined.
#
#   scripts/install-hooks.sh
#
# Idempotent, and safe on a clone that already has them.

set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"

git config core.hooksPath .githooks
echo "hooks: core.hooksPath -> .githooks"

# The deny list holds real names and lives outside the tree, by design: a list of
# colleagues' names committed to a public repository is the leak it exists to
# prevent. Seed it from the template when it is missing so the file exists and its
# format is documented where the operator will look.
deny=${TA_COMMIT_DENY:-$HOME/.config/tab-atelier/commit-deny.txt}
if [ -f "$deny" ]; then
	echo "deny list: $deny (already present)"
else
	mkdir -p "$(dirname "$deny")"
	cp scripts/commit-deny.example.txt "$deny"
	chmod 600 "$deny"
	printf 'deny list: %s (created from the template — add the names to keep out)\n' "$deny"
fi

# A reviewer needs the agent. A missing binary refuses rather than passes, so say
# it now instead of at the first push.
if [ -x target/release/catbus-agent ] || command -v catbus-agent >/dev/null 2>&1; then
	echo "reviewer: catbus-agent found"
else
	echo "reviewer: no catbus-agent yet — build it before pushing:" >&2
	echo "    cargo build -p catbus-agent --release" >&2
fi
