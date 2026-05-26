#!/usr/bin/env bash
# Install GitHub Next's Monaspace v1.400 (variable build) into the
# user's font dir and refresh the fontconfig cache. Variable build
# lets gpui interpolate any `ui_font_weight` value (e.g. 250) across
# every glyph uniformly — without it, fontconfig rounds to the
# nearest static face per-glyph, and rarely-used codepoints (€, —,
# …) end up in a different face than the digits next to them, which
# reads as "uneven bold" in the terminal.
#
# Idempotent: re-running upgrades / replaces the existing files in
# place. Stays inside $HOME — no sudo required.
#
# After running, switch your editor / tab-atelier font config to
#     "ui_font_family": "Monaspace Neon Var"
# (note the trailing " Var"), restart the app, and the uneven-bold
# rendering should be gone.

set -euo pipefail

VERSION="${MONASPACE_VERSION:-v1.400}"
URL="https://github.com/githubnext/monaspace/releases/download/${VERSION}/monaspace-variable-${VERSION}.zip"

FONT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/fonts/Monaspace"
TMPDIR="$(mktemp -d -t monaspace-XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "→ Downloading Monaspace ${VERSION} (variable build)..."
curl --fail --location --progress-bar --output "$TMPDIR/monaspace.zip" "$URL"

echo "→ Extracting..."
unzip -q "$TMPDIR/monaspace.zip" -d "$TMPDIR/unzipped"

# The zip layout is monaspace-variable-vX.Y/fonts/variable/*.ttf.
# Grab every .ttf under the unzipped tree to stay forwards-compatible
# with any future restructure.
mkdir -p "$FONT_DIR"
mapfile -t TTFS < <(find "$TMPDIR/unzipped" -type f -name '*.ttf')
if [[ ${#TTFS[@]} -eq 0 ]]; then
    echo "ERROR: no .ttf files inside the zip — release layout changed?" >&2
    exit 1
fi
echo "→ Installing ${#TTFS[@]} font file(s) to $FONT_DIR ..."
for ttf in "${TTFS[@]}"; do
    cp -f "$ttf" "$FONT_DIR/"
done

echo "→ Refreshing fontconfig cache..."
if command -v fc-cache >/dev/null 2>&1; then
    fc-cache -f "$FONT_DIR" >/dev/null
else
    echo "  fc-cache not found in PATH; skipping. Restart your apps to pick up the new fonts." >&2
fi

echo
echo "✓ Monaspace ${VERSION} variable installed."
echo
echo "Verify with:"
echo "  fc-match -f '%{fullname} %{file}\\n' 'Monaspace Neon Var'"
echo
echo "Then in your editor / tab-atelier-readable Zed config, set:"
echo '  "ui_font_family": "Monaspace Neon Var"'
echo '  "ui_font_weight": 250'
echo "and restart the app."
