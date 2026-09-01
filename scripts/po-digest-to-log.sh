#!/usr/bin/env bash
#
# po-digest-to-log.sh — KIOSK PD4: the CANONICAL "decisions → decision log" producer.
#
# Writes each pending PO decision as ONE `open` line into
# ~/.tab-atelier/decisions.jsonl (the native cross-project cold source) via
# `tab-atelier decision push`. Idempotent by construction: `decision push` dedups an
# identical re-run (same content on an open decision) — so this is safe to run on a
# timer / on PO silence without growing the log.
#
# This is the REVIEWABLE + TESTABLE canonical logic (it lives in the repo). The
# Botmox `po-digest.sh` (not a git repo) DELEGATES to this script — that wiring is
# out-of-repo. The GitHub #1/#2 digest issues stay an OPTIONAL mirror; this native log
# is the source.
#
# Input (default, --project harness): TSV on stdin, one decision per line:
#   id <TAB> title <TAB> why <TAB> reco <TAB> effort <TAB> files(comma-separated)
# Empty fields are allowed (only `id` + `title` matter for a useful card).
#
# Modes:
#   --project <p>   the decision's project (harness | kalpin | graines). Default harness.
#   --glab          kalpin variant: pull `label:decision` issues from an authenticated
#                   `glab` and push each (project=kalpin) into the SAME log. Needs glab+jq.
#
# The tab-atelier binary is `$TAB_ATELIER_BIN` (default `tab-atelier`) — the test points
# it at the built binary; the log path honors `$TAB_ATELIER_DECISIONS_PATH`.

set -euo pipefail

project="harness"
mode="stdin"
tabatelier="${TAB_ATELIER_BIN:-tab-atelier}"

while [ $# -gt 0 ]; do
    case "$1" in
        --project) project="$2"; shift 2 ;;
        --glab) mode="glab"; project="kalpin"; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "po-digest-to-log: unexpected arg '$1'" >&2; exit 2 ;;
    esac
done

# The single canonical mapping: a decision's fields → one `decision push` call. Every
# producer path (stdin / glab) funnels through HERE, so the schema stays in one place.
push_one() {
    local id="$1" title="$2" why="$3" reco="$4" effort="$5" files="$6"
    [ -n "$id" ] || return 0
    local args=(decision push --id "$id" --project "$project")
    [ -n "$title" ]  && args+=(--title "$title")
    [ -n "$why" ]    && args+=(--why "$why")
    [ -n "$reco" ]   && args+=(--reco "$reco")
    [ -n "$effort" ] && args+=(--effort "$effort")
    [ -n "$files" ]  && args+=(--files "$files")
    "$tabatelier" "${args[@]}"
}

if [ "$mode" = "glab" ]; then
    # kalpin: same schema, same log, project=kalpin. Each `decision`-labelled issue → one
    # open. Requires an authenticated `glab` + `jq`. (The harness path below is the one the
    # rust round-trip test exercises; this branch reuses the same `push_one` mapping.)
    command -v glab >/dev/null 2>&1 || { echo "po-digest-to-log --glab: glab not found" >&2; exit 3; }
    command -v jq   >/dev/null 2>&1 || { echo "po-digest-to-log --glab: jq not found" >&2; exit 3; }
    glab issue list --label decision --output json 2>/dev/null \
        | jq -r '.[] | [(.iid|tostring), .title, "", "", "", ""] | @tsv' \
        | while IFS=$'\t' read -r id title why reco effort files; do
              push_one "kb-$id" "$title" "$why" "$reco" "$effort" "$files"
          done
    exit 0
fi

# harness (default): TSV on stdin. `|| [ -n "$id" ]` processes a final line that lacks a
# trailing newline (read returns non-zero on EOF but still fills the vars).
while IFS=$'\t' read -r id title why reco effort files || [ -n "${id:-}" ]; do
    push_one "${id:-}" "${title:-}" "${why:-}" "${reco:-}" "${effort:-}" "${files:-}"
done
