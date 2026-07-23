#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Claude Code PostToolUse hook (see .claude/settings.json): after every
# Write/Edit of a Rust file, auto-format the owning package and run the
# same strict clippy CI runs. Clippy failures exit 2, which feeds the
# errors straight back to Claude so they get fixed in the same turn
# instead of surfacing later in CI.
set -u

command -v jq >/dev/null 2>&1 || exit 0
command -v cargo >/dev/null 2>&1 || exit 0

file=$(jq -r '.tool_input.file_path // .tool_response.filePath // empty')
case "$file" in
*.rs) ;;
*) exit 0 ;;
esac

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

# Map the edited file to the workspace package that owns it. The root
# package is linted with CI's headless feature set so the hook works in
# environments without the GUI system libraries (the GUI feature set is
# still covered by CI).
rel=${file#"$PWD"/}
clippy_extra=()
case "$rel" in
crates/catbus-agent/*) pkg=catbus-agent ;;
android/*) exit 0 ;; # separate cargo project, not a workspace member
*) pkg=tab-atelier clippy_extra=(--no-default-features --features headless,energy) ;;
esac

cargo fmt -p "$pkg" 2>/dev/null

if ! out=$(cargo clippy -p "$pkg" "${clippy_extra[@]}" --all-targets 2>&1); then
    printf '%s\n' "$out" | tail -n 60 >&2
    exit 2
fi
exit 0
