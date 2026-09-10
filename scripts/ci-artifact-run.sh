#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Print the run id whose artifacts the publish job should stage for a given
# workflow — preferring the run for THIS commit, waiting for it if it is still
# going, and falling back to the newest successful run otherwise.
#
#     RUN_ID=$(scripts/ci-artifact-run.sh arch-pkg.yml)
#     RUN_ID=$(scripts/ci-artifact-run.sh windows-msi.yml 0)   # never wait
#
# Needs GH_TOKEN and GITHUB_REPOSITORY in the environment. Prints the id on
# stdout (empty if there is nothing usable) and its reasoning on stderr.
#
# THE RACE THIS EXISTS FOR. apt-publish, arch-pkg and android-apk all trigger
# on the same push. The publish job used to ask for "the latest successful
# arch-pkg run", which at that moment is the PREVIOUS commit's — the current
# one is still compiling. So the pacman repo sat exactly one commit behind
# main, forever, and nobody noticed because every workflow was green.
#
# It surfaced when the fleet handbook was added to the Arch package: the build
# was fixed and passing, and an Arch user still had no docs, because what was
# being published was the build from the commit before the fix.
#
# Waiting costs the publish job the tail of the sibling build (arch-pkg is the
# long one, ~15 min). That is the price of publishing the commit you just
# pushed rather than the one before it.
#
# NOT EVERY WORKFLOW RUNS ON EVERY PUSH — windows-msi is tag-only. When no run
# exists for this commit there is nothing to wait for, so we fall straight back
# to the newest successful run and the MSI keeps being republished as-is.
set -euo pipefail

workflow="${1:?usage: $0 <workflow-file.yml> [max-wait-seconds]}"
max_wait="${2:-1800}"
poll=30

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
sha="${GITHUB_SHA:-}"

latest_successful() {
    gh run list --repo "$repo" --workflow="$workflow" --status=success \
        -L 1 --json databaseId --jq '.[0].databaseId // empty'
}

# `gh run list` has no headSha filter, so pull a page and match locally. 30 is
# comfortably more than the number of runs that can pile up on one commit.
run_for_sha() {
    gh run list --repo "$repo" --workflow="$workflow" -L 30 \
        --json databaseId,headSha,status,conclusion \
        --jq "[.[] | select(.headSha == \"$sha\")] | .[0] // empty"
}

if [ -z "$sha" ]; then
    echo "$0: no GITHUB_SHA — using the newest successful $workflow run" >&2
    latest_successful
    exit 0
fi

waited=0
while :; do
    run="$(run_for_sha)"
    if [ -z "$run" ]; then
        echo "$0: no $workflow run for $sha (it may not trigger on this event)" >&2
        break
    fi

    id="$(printf '%s' "$run" | jq -r '.databaseId')"
    status="$(printf '%s' "$run" | jq -r '.status')"
    conclusion="$(printf '%s' "$run" | jq -r '.conclusion // ""')"

    if [ "$status" = completed ]; then
        if [ "$conclusion" = success ]; then
            echo "$0: $workflow run $id built this commit — publishing that" >&2
            echo "$id"
            exit 0
        fi
        # A failed sibling must not block the publish: the rest of the site
        # still needs regenerating. Republish what we already had.
        echo "$0: $workflow run $id for this commit ended '$conclusion' — falling back" >&2
        break
    fi

    if [ "$waited" -ge "$max_wait" ]; then
        echo "$0: $workflow run $id still '$status' after ${waited}s — falling back" >&2
        break
    fi
    echo "$0: waiting for $workflow run $id ($status, ${waited}s/${max_wait}s)…" >&2
    sleep "$poll"
    waited=$((waited + poll))
done

fallback="$(latest_successful)"
if [ -n "$fallback" ]; then
    echo "$0: staging the newest successful $workflow run $fallback instead" >&2
else
    echo "$0: no successful $workflow run at all — keeping what is published" >&2
fi
echo "$fallback"
