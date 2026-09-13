# shellcheck shell=bash
# SPDX-License-Identifier: MPL-2.0
#
# Sourced by the wrappers in this directory. It holds the two facts about this
# machine that every wrapper would otherwise have to remember.

# Debian ships cargo 1.85 in /usr/bin, which cannot build this tree (rust-version
# is 1.92). The rustup install in ~/.cargo/bin is not on PATH in a non-login
# shell, so prepend it.
export PATH="$HOME/.cargo/bin:$PATH"

# Pinned so a wrapper behaves the same whatever rustup's default toolchain is.
# Override with TA_TOOLCHAIN; an empty value means "use the default".
TA_TOOLCHAIN="${TA_TOOLCHAIN-1.95.0}"
if [ -n "$TA_TOOLCHAIN" ]; then
  cargo() { command cargo "+$TA_TOOLCHAIN" "$@"; }
fi
