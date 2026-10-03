#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Run the desktop dashboard's unit tests.
#
# Same shape as `crates/tab-atelier-kiosk/assets/run-tests.sh`, and for the same
# reason: plain ES modules, `node <name>.test.mjs`, no framework, no dependency,
# no package.json. This repository is Rust and its UI is embedded assets; an npm
# toolchain to run a dozen assertion files would cost every contributor more than
# it saves.
#
# The acceptance tests (`*.accept.mjs`) are NOT run here: they drive a real
# browser and need playwright, which this repository does not carry. They are for
# a runner that ships its own node_modules.
#
# Why this file exists at all: these tests arrived with the app.rs split and
# nothing ran them — not this repository's CI, not that branch's. A test no one
# runs is a comment. This is what makes them a check.
#
# Exits non-zero if any test fails, so it can gate CI.

set -eu

# Run from the assets directory so the relative `./dashboard.js` imports resolve
# the same way wherever this is invoked from.
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"

if ! command -v node >/dev/null 2>&1; then
    echo "node is required to run the dashboard tests" >&2
    exit 1
fi

failed=0
count=0
for test_file in dashboard.*.test.mjs; do
    [ -e "$test_file" ] || continue
    count=$((count + 1))
    name=$(basename "$test_file")
    if node "$test_file"; then
        echo "ok   - $name"
    else
        echo "FAIL - $name" >&2
        failed=$((failed + 1))
    fi
done

if [ "$count" -eq 0 ]; then
    echo "no dashboard tests found" >&2
    exit 1
fi

echo "$((count - failed))/$count passed"
[ "$failed" -eq 0 ]
