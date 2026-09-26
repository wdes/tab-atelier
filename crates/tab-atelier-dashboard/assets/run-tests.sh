#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Run the dashboard's unit tests.
#
# They are plain ES modules: `node tests/<name>.test.mjs`, no framework, no
# dependency, no package.json. Deliberately so — this repository is Rust, and
# the UI is embedded assets; adding an npm toolchain to run a handful of
# assertions would cost every contributor more than it saves.
#
# The acceptance tests (`*.accept.mjs`) are NOT run here: they drive a real
# browser and need playwright, which is not in this repository. They are a
# separate concern, for a runner that ships its own node_modules.
#
# Exits non-zero if any test fails, so it can gate CI.

set -eu

# Run from the assets directory so the relative `../dashboard.js` imports resolve
# the same way wherever this is invoked from.
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"

if ! command -v node >/dev/null 2>&1; then
    echo "node is required to run the dashboard tests" >&2
    exit 1
fi

failed=0
count=0
for test_file in tests/*.test.mjs; do
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
