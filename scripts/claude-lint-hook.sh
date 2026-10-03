#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Claude Code PostToolUse hook (see .claude/settings.json): after every
# Write/Edit of a Rust file, format it.
#
# It used to run `cargo clippy --fix` and then a full strict clippy pass on
# every edit. That is two clippy invocations against this workspace's whole
# dependency graph — minutes per file, with the timeout raised to 600s to
# accommodate it. The feedback was useful but the cost dominated a session.
# Clippy now runs before every commit and in CI (`--workspace --all-targets
# -- -D warnings`), which is where it belongs.
#
# What remains is `rustfmt` on the edited file, which is ~30ms. It guards a
# real failure: CI rejects formatting drift. Running the formatter directly
# rather than through cargo skips workspace resolution, and the edition comes
# from rustfmt.toml, so nothing has to be passed or kept in sync here.
set -u

command -v jq >/dev/null 2>&1 || exit 0

file=$(jq -r '.tool_input.file_path // .tool_response.filePath // empty')
case "$file" in
*.rs) ;;
*) exit 0 ;;
esac

# The desktop build under android/ is a separate cargo project with its own
# toolchain and formatting expectations; not this hook's business.
case "$file" in
*/android/*) exit 0 ;;
esac

# Same toolchain as CI and the other wrappers, stated once in lib/.
# shellcheck source=scripts/lib/toolchain.sh
. "$(dirname "$0")/lib/toolchain.sh" 2>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"

# Format the file in place. The absence of `--edition` is deliberate: rustfmt
# finds rustfmt.toml by walking up from the file, and that file is where the
# edition is declared.
rustfmt ${TA_TOOLCHAIN:+"+$TA_TOOLCHAIN"} "$file" >/dev/null 2>&1

exit 0
