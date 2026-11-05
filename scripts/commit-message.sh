#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Rewrite a commit message through a sub-agent, then gate it deterministically.
#
# Two jobs, and the order matters:
#
#   1. THE GATE, which is not an LLM. It looks for the shapes that must never
#      reach a public history — email addresses, absolute paths, the machine's
#      own hostname, non-ASCII bytes, key material, and any name listed in the
#      operator's deny file. Deterministic, so it cannot be talked out of it.
#
#   2. THE REWRITE, through `claude -p`, which turns a draft into a message that
#      says what changed and why without the investigation that found it.
#
# Written because two commits carried colleagues' first names into a public
# repository: the text had been copied out of a bug report because it made the
# commit read well. The gate makes that a refusal; the rewrite makes it not
# happen in the first place.
#
#   usage: commit-message.sh <path-to-COMMIT_EDITMSG> [--lint-only]
#          commit-message.sh --check-rev <git-range>
#
# There is no bypass: nothing in the environment disables either step, and
# rebases and cherry-picks are checked like any other commit. `--check-rev`
# exists because `git commit --no-verify` skips hooks altogether, so CI runs the
# gate over every pushed message instead of trusting that the hook ran.
#
# Environment:
#   TA_COMMIT_MSG_MODEL      model for the rewrite (default: the CLI's own)
#   TA_COMMIT_MSG_TIMEOUT    seconds to allow the rewrite (default: 120)
#   TA_COMMIT_DENY           deny file path (default: see deny_file below)

set -u

msg_file=${1:-}
mode=${2:-}

# Range mode: gate every message in a commit range, for CI. No rewriting — the
# commits are already written, and changing them here would rewrite published
# history.
if [ "$msg_file" = "--check-rev" ]; then
	range=${mode:?usage: commit-message.sh --check-rev <range>}
	rc=0
	for sha in $(git rev-list "$range" 2>/dev/null); do
		f=$(mktemp) || exit 1
		git log -1 --format=%B "$sha" >"$f"
		if ! TA_RANGE_SCAN=1 "$0" "$f" --lint-only 2>/tmp/ta-commit-msg.$$; then
			printf '%s: %s\n' "$(git log -1 --format='%h %s' "$sha")" "$(cat /tmp/ta-commit-msg.$$)" >&2
			rc=1
		fi
		rm -f "$f"
	done
	rm -f /tmp/ta-commit-msg.$$
	exit $rc
fi

[ -n "$msg_file" ] || exit 0
[ -f "$msg_file" ] || exit 0

# Nothing but comments and blanks: an empty commit, which git aborts on its own.
grep -qvE '^[[:space:]]*(#|$)' "$msg_file" || exit 0

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

# Names that must never appear. The file is not committed — it holds the real
# names of the operator's colleagues and servers, which is exactly the material
# this exists to keep out of git.
deny_file=${TA_COMMIT_DENY:-$HOME/.config/tab-atelier/commit-deny.txt}

# The hostname this runs on, in both the short and the fully-qualified form. The
# name is read at run time and never written down here: a comment that spells it
# out is the same leak the check exists to catch, and this line was caught by it.
# Skipped when it is a bare container id or empty.
host_short=$(hostname -s 2>/dev/null || true)
host_full=$(hostname -f 2>/dev/null || true)

gate() {
	violations=''

	# Trailer lines are the repository's own published identities, appended by this
	# hook after the message was written. They are not quoted content, and refusing
	# them would refuse every commit; they are removed before the email check.
	subject=$(grep -viE '^(Co-authored-by|Signed-off-by|Reviewed-by):' "$msg_file" || true)

	# Non-ASCII. A diacritic is the case that started this: the same name has two
	# Unicode spellings, and a byte-level scan cannot tell whether it is a person
	# or a typo. Refusing the class is simpler and safer than judging instances.
	#
	# Typographic punctuation is normalised away first, because an em dash is not
	# a name and refusing every commit that uses one would train people to bypass
	# the hook. What survives that normalisation is text someone pasted, which is
	# exactly what must not be published.
	#
	# `[:print:]` under LC_ALL=C is exactly 0x20–0x7E. `grep -P` would be tidier
	# and is not available everywhere.
	if sed 's/—/-/g; s/–/-/g; s/…/.../g; s/’/'"'"'/g; s/‘/'"'"'/g; s/“/"/g; s/”/"/g' "$msg_file" |
		LC_ALL=C grep -q '[^[:print:][:space:]]'; then
		violations="$violations
  non-ASCII text — a name or text copied from elsewhere"
	fi

	# An email address. Trailers are appended by the caller AFTER this runs, so
	# anything here is quoted content.
	if printf '%s\n' "$subject" | grep -qE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'; then
		violations="$violations
  email address"
	fi

	# Absolute host paths: they name the operator's machine and its layout.
	if grep -qE '(/var|/etc|/home|/root|/srv|/opt|/mnt|/usr|/run)/' "$msg_file"; then
		violations="$violations
  absolute path — say \"the config directory\" instead"
	fi

	# This machine's own name.
	for h in "$host_short" "$host_full"; do
		[ -n "$h" ] || continue
		# A one- or two-character name would match ordinary prose.
		[ ${#h} -ge 3 ] || continue
		if grep -qiF -- "$h" "$msg_file"; then
			violations="$violations
  this machine's hostname ($h)"
		fi
	done

	# Key material.
	if grep -qE 'sk-[A-Za-z0-9_-]{8,}|sk-ant-[A-Za-z0-9_-]{8,}' "$msg_file"; then
		violations="$violations
  what looks like an API key"
	fi

	# The operator's own list of names to keep out. This is the only part of the
	# gate that can catch a person's name: a person is not a syntactic shape, and
	# a linter that guessed at capitalised words would refuse "Rust" and "Claude"
	# along with the names it should refuse.
	if [ -f "$deny_file" ]; then
		while IFS= read -r name || [ -n "$name" ]; do
			case $name in '' | '#'*) continue ;; esac
			[ ${#name} -ge 3 ] || continue
			if grep -qiF -- "$name" "$msg_file"; then
				violations="$violations
  a name from $deny_file"
			fi
		done <"$deny_file"
	elif [ "$mode" != "--lint-only" ]; then
		# Said once per commit, not as a failure: an empty list is legitimate,
		# but silence would let the operator believe names are covered when the
		# file has never been created.
		printf 'commit-msg: no deny list at %s — names are not being checked\n' \
			"$deny_file" >&2
	fi

	# Storytelling is refused only while drafting. In a range scan these are
	# already-published messages that cannot be un-written, and refusing them would
	# block every push for history nobody can now change. Leakage is absolute;
	# narration is a drafting guideline. This skips the two checks below and still
	# reports whatever the leak checks found above.
	if [ "${TA_RANGE_SCAN:-0}" != "1" ]; then
		# Storytelling: the message is a report about the work, not the work. These
		# are the openers that reliably mean the text drifted into narrative.
		# Word boundaries matter: `^we` matched "were checked", `^so` matched
		# "something", and the gate refused a message that was already fine. A
		# check that fires on innocent prose gets disabled by whoever it blocks,
		# so it has to be precise about what a first word is.
		if grep -qniE '^(I|we|so|okay|well)([[:space:],.:;!?]|$)|^today[[:space:]]|^this session|^as (requested|you asked)|^the user (reported|asked|wants)' "$msg_file"; then
			violations="$violations
  first person or a report about the conversation — state the change, not the story"
		fi
		if grep -qE '^> ' "$msg_file"; then
			violations="$violations
  quoted text — describe it, do not paste it"
		fi
	fi

	if [ -n "$violations" ]; then
		printf 'commit-msg: the message carries material that must not be published:%s\n' "$violations" >&2
		return 1
	fi
	return 0
}

if [ "$mode" = "--lint-only" ]; then
	gate
	exit $?
fi

# There is no bypass. No environment variable turns this off, and rebases and
# cherry-picks are not exempt: rewriting history is precisely when a leaked name
# travels furthest, so those runs get the gate more urgently than a fresh commit,
# not less. `git commit --no-verify` skips this hook entirely — which is why
# `--check-rev` exists below and runs in CI over every pushed commit.

# ---------------------------------------------------------------------------
# The rewrite
# ---------------------------------------------------------------------------

# What the sub-agent is allowed to see: the draft, the branch, and the names of
# the files that changed. NOT the diff — a diff is exactly where the operator's
# paths, keys and hostnames live, and none of that is needed to say what changed.
changed=$(git diff --cached --name-status 2>/dev/null | head -40 || true)

prompt=$(cat <<EOF
You rewrite a git commit message. Output the message and nothing else — no
preamble, no code fence, no explanation.

HARD RULES, checked afterwards by a script that will reject the commit:
- No person's name, no server or hostname, no company or project name taken from
  the draft. Write "provider A", "the relay host", "the config directory".
- No email address, no absolute path, no API key, no quoted text from a bug
  report or log. Describe the shape of the data, never the data.
- ASCII only.
- No storytelling: no "I", "we", "as requested", "the user reported", "this
  session", "after investigating". Do not narrate the search, do not thank
  anyone, do not summarise the conversation. Say what changed and why it
  mattered, then stop.

FORMAT:
- Subject: conventional commit, lower case, no full stop, at most 72 characters:
    type(scope): summary
  type is one of feat, fix, refactor, docs, test, chore, perf, build, ci, revert.
  scope is the crate or area (proxy, dashboard, daemon, android, ...).
- Blank line, then a body wrapped at 72 columns.
- The body explains the cause and the consequence. Prefer two or three short
  paragraphs, or "-" bullets when listing. No trailer lines; another hook adds
  them.

FILES THAT CHANGED:
$changed

DRAFT MESSAGE (rewrite it; keep the facts, drop the story):
---8<---
$(cat "$msg_file")
---8<---
EOF
)

timeout_s=${TA_COMMIT_MSG_TIMEOUT:-120}

tmp=$(mktemp) || exit 1
trap 'rm -f "$tmp"' EXIT

prompt_file=$(mktemp) || exit 1
printf '%s\n' "$prompt" >"$prompt_file"

here=$(cd "$(dirname "$0")" && pwd)
# The writer identity, not the reviewer one: `catbus-ask.sh` defaults to the
# reviewer, and handing a rewrite job to an agent told to answer with a verdict
# produces exactly that — a verdict where a message should be.
CATBUS_IDENTITY_FILE=${CATBUS_IDENTITY_FILE:-$here/catbus-writer.md}
export CATBUS_IDENTITY_FILE
rewritten=$("$here/catbus-ask.sh" "$timeout_s" <"$prompt_file") || rewritten=''
rm -f "$prompt_file"

# A failed or empty rewrite falls back to the draft — the gate below still runs,
# so the commit is never allowed through just because the model was unavailable.
if printf '%s' "$rewritten" | grep -qvE '^[[:space:]]*$'; then
	printf '%s\n' "$rewritten" >"$tmp"
	# Strip a fence if one came back anyway; the rules above asked for none.
	sed -i '/^```/d' "$tmp"
	mv "$tmp" "$msg_file"
	trap - EXIT
else
	printf 'commit-msg: the rewrite produced nothing; gating the draft instead\n' >&2
	exit_gate=0
	gate || exit_gate=1
	exit $exit_gate
fi

gate
