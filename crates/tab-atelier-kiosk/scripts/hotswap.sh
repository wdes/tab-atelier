#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Hotswap the kiosk binary from this working tree — no .deb, no apt, no CI wait.
#
# The Kiosk is a standalone server, so iterating on it does not need a package:
# build the release binary, run its tests, copy it over /usr/bin/tab-atelier-kiosk
# and restart the unit. A change is live in seconds. Packaging exists to SHIP the
# kiosk to someone else, not to test your own edit.
#
# Usage:
#   scripts/hotswap.sh              # build, test, install, restart
#   scripts/hotswap.sh --no-test    # skip the test run (faster, for a quick look)
#
# It writes /usr/bin and restarts the system unit, so it uses sudo — you will be
# asked for your password. Run it from anywhere; it finds the repo itself.
#
# The installed binary belongs to the package: a later `apt install` overwrites
# it, which is fine — just run this again after pulling.
set -euo pipefail

crate="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$crate/../.." && pwd)"
target="${CARGO_TARGET_DIR:-$repo/target}"

# The pinned toolchain lives in ~/.cargo/bin; a bare `cargo` is often too old to
# build this tree. Put it on PATH if it is not already.
case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) ;;
  *) export PATH="$HOME/.cargo/bin:$PATH" ;;
esac

run_tests=1
for arg in "$@"; do
  case "$arg" in
    --no-test) run_tests=0 ;;
    -h | --help)
      echo "usage: $0 [--no-test]"
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

cd "$repo"

echo "== build (release) =="
cargo build --release -p tab-atelier-kiosk

if [ "$run_tests" = 1 ]; then
  echo "== tests =="
  cargo test -p tab-atelier-kiosk
fi

bin="$target/release/tab-atelier-kiosk"
[ -x "$bin" ] || {
  echo "no binary at $bin" >&2
  exit 1
}

echo "== install $bin -> /usr/bin/tab-atelier-kiosk =="
sudo install -m 0755 "$bin" /usr/bin/tab-atelier-kiosk

# The UI is served from a web root, not compiled in, so the files go beside the
# install too — the package does this with its asset list; a hotswap copies them.
echo "== install the UI -> /usr/share/tab-atelier-kiosk/web =="
sudo install -d -m 0755 /usr/share/tab-atelier-kiosk/web
sudo cp "$crate"/assets/kiosk.html "$crate"/assets/kiosk.css "$crate"/assets/kiosk.js \
        "$crate"/assets/manage.html "$crate"/assets/manage.css "$crate"/assets/manage.js \
        /usr/share/tab-atelier-kiosk/web/

echo "== restart the unit =="
sudo systemctl restart tab-atelier-kiosk
if systemctl is-active --quiet tab-atelier-kiosk; then
  echo "ok: tab-atelier-kiosk is running on $(systemctl show -p ExecStart --value tab-atelier-kiosk | sed 's/.*--listen //; s/;.*//')"
else
  echo "the unit is not active — check: systemctl status tab-atelier-kiosk" >&2
  exit 1
fi
