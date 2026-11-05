#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Review everything that is about to be pushed, before it is pushed.
#
# The commit-msg hook sees the message. It does not see the CODE, and the code is
# where the same material leaks just as easily: a doc comment naming a colleague,
# a test fixture built from the operator's real config, a path copied out of a log.
# That is not hypothetical here — one leak was a commit message and the other was
# four lines of a Rust file.
#
# So this gates `origin/main..HEAD` — everything since the last push — in two
# passes, in this order for a reason:
#
#   1. A DETERMINISTIC scan of added lines. Emails, absolute paths, this machine's
#      hostname, key material, non-ASCII, the operator's deny list. It runs first
#      and it refuses BEFORE any diff leaves the machine, because sending a diff to
#      a model to ask "does this contain a secret" is itself a disclosure. An
#      address under example.com, example.net or example.org is not an email for
#      this purpose: RFC 6761 reserves those for documentation.
#
#   2. A SUB-AGENT review of what survived. It reads the added lines and answers
#      with the specific hunks that carry a person, a server or a private name.
#      A separate process with no conversation context: the point is a second
#      reader, not a second opinion from the author.
#
# No bypass. `git push --no-verify` skips hooks, which is why the same script runs
# in CI over the pushed range — see `.github/workflows/build.yml`.
#
#   usage: commit-review.sh [<base>]     default base: origin/main
#
# Environment:
#   TA_REVIEW_MODEL      model for the agent pass (default: the CLI's own)
#   TA_REVIEW_TIMEOUT    seconds for the agent pass (default: 180)
#   TA_REVIEW_DENY       deny file path (default: see deny_file below)
#   TA_REVIEW=off        NOT SUPPORTED — there is no bypass

set -u

base=${1:-origin/main}
deny_file=${TA_REVIEW_DENY:-$HOME/.config/tab-atelier/commit-deny.txt}
here=$(cd "$(dirname "$0")" && pwd)
# Names already public, so a scan of a deny list does not have to exempt them by
# hard-coded special case. Committed, because it is data about public names.
allow_file=${TA_REVIEW_ALLOW:-$here/commit-allow.txt}
timeout_s=${TA_REVIEW_TIMEOUT:-180}

git rev-parse --verify "$base" >/dev/null 2>&1 || {
	echo "review: no such base $base" >&2
	exit 1
}

# Added lines only. A deletion is not a leak: it is the fix.
diff=$(git diff "$base"...HEAD --no-color --no-ext-diff --unified=0 2>/dev/null) || {
	echo "review: could not read the diff" >&2
	exit 1
}
[ -n "$diff" ] || exit 0

host_short=$(hostname -s 2>/dev/null || true)
host_full=$(hostname -f 2>/dev/null || true)

# ---------------------------------------------------------------------------
# 1. Deterministic scan, before the diff goes anywhere
# ---------------------------------------------------------------------------

added=$(printf '%s\n' "$diff" | grep -E '^\+' | grep -v '^+++')
[ -n "$added" ] || exit 0

problems=''

# Non-ASCII, minus typographic punctuation: a diacritic in a comment or fixture is
# the exact shape of the leak this exists to stop.
if printf '%s\n' "$added" |
	sed 's/—/-/g; s/–/-/g; s/…/.../g; s/’/'"'"'/g; s/‘/'"'"'/g; s/“/"/g; s/”/"/g' |
	LC_ALL=C grep -q '[^[:print:][:space:]]'; then
	problems="$problems
  non-ASCII text in an added line"
fi

# Emails. Two kinds are cleared, and only two:
#
#   - the ones this repo already publishes in its own manifests and trailers,
#     which were there before the rule and are the maintainer's own;
#   - addresses under `example.com`, `example.net` and `example.org`, which
#     RFC 6761 and the IANA special-use registry reserve for documentation, so
#     they cannot name anyone. A fixture needs *an* address, and this is the
#     only kind that is guaranteed not to be someone's.
#
# The domain is matched whole, subdomains included (`shop.example.com` is
# reserved too), and never as a suffix: `example.com.evil.net` is a real
# domain that merely starts with a reserved one, and `notexample.com` is not
# reserved at all.
if printf '%s\n' "$added" |
	grep -oE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' |
	grep -vE '^<[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+>$' |
	grep -ivE '@([A-Za-z0-9-]+\.)*example\.(com|net|org)\.?$' |
	grep -q .; then
	problems="$problems
  an email address in an added line"
fi

# Absolute host paths in added lines. The repo's own docs use `/usr/share`, so this
# targets the private ones: a config or state directory under /var/lib, /home or
# /root is the operator's machine.
if printf '%s\n' "$added" | grep -qE '/(var/lib|var/cache|home|root|srv|mnt)/'; then
	problems="$problems
  a private absolute path in an added line"
fi

for h in "$host_short" "$host_full"; do
	[ -n "$h" ] || continue
	[ ${#h} -ge 3 ] || continue
	if printf '%s\n' "$added" | grep -qiF -- "$h"; then
		problems="$problems
  this machine's hostname in an added line"
	fi
done

if printf '%s\n' "$added" | grep -qE 'sk-[A-Za-z0-9_-]{16,}'; then
	problems="$problems
  what looks like a real API key"
fi

if [ -f "$deny_file" ]; then
	# Public names are removed from the text first: a deny entry that happens to
	# contain a vendor name would otherwise fire on every mention of it.
	scrubbed=$added
	if [ -f "$allow_file" ]; then
		while IFS= read -r ok || [ -n "$ok" ]; do
			case $ok in '' | '#'*) continue ;; esac
			[ ${#ok} -ge 3 ] || continue
			scrubbed=$(printf '%s\n' "$scrubbed" | sed "s/$ok//gI")
		done <"$allow_file"
	fi
	while IFS= read -r name || [ -n "$name" ]; do
		case $name in '' | '#'*) continue ;; esac
		[ ${#name} -ge 3 ] || continue
		if printf '%s\n' "$scrubbed" | grep -qiF -- "$name"; then
			problems="$problems
  a name from $deny_file"
		fi
	done <"$deny_file"
fi

if [ -n "$problems" ]; then
	cat >&2 <<EOF
review: refusing to push — the diff since $base carries material that must not be
published:$problems

Fix the lines (not the check), then push again. Nothing was sent to a model.
EOF
	exit 1
fi

# ---------------------------------------------------------------------------
# 2. Sub-agent review of what survived
# ---------------------------------------------------------------------------

# This pass exists because a person's first name is not a syntactic shape: the
# deterministic scan above cannot tell "Ada" from "ada", and guessing at
# capitalised words would refuse "Rust", "Claude" and "Debian". A reader can see
# that a name is attached to a provider entry, a fixture or a comment about an
# account, which is the thing that matters.
here=$(cd "$(dirname "$0")" && pwd)
prompt_file=$(mktemp) || exit 1
trap 'rm -f "$prompt_file"' EXIT

cat >"$prompt_file" <<EOF
Review this diff before it is pushed. Added lines only; a deletion is the fix, not
the leak.

$diff
EOF

verdict=$("$here/catbus-ask.sh" "$timeout_s" <"$prompt_file") || {
	# A review that could not run is not a pass. The deterministic scan above has
	# already cleared the shapes it knows; this is the one that needs a reader, so
	# failing open here would quietly disable it for everyone.
	echo "review: the agent pass did not run — refusing to push without a review" >&2
	exit 1
}

# A verdict without the per-check lines is an impression, not a review. Requiring
# them found a real difference: asked to decide directly, the agent answered
# `clean` on a diff it had called a leak a minute earlier, because the same prompt
# read as a judgement call rather than a checklist.
case "$verdict" in
*"CHECK "*"VERDICT: "*) ;;
*)
	printf 'review: the agent answered without the per-check lines, so it is not a review:\n%s\n' "$verdict" >&2
	exit 1
	;;
esac

case "$verdict" in
*"VERDICT: clean"*)
	exit 0
	;;
*"VERDICT: leak"*)
	printf 'review: the agent found material that must not be published:\n%s\n' "$verdict" >&2
	exit 1
	;;
*)
	printf 'review: the agent answer was not a verdict, so it is not a pass:\n%s\n' "$verdict" >&2
	exit 1
	;;
esac
