#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Render the fleet's audit reports from the blackboard into docs/audit-findings.md.

The board is the source of truth — every `done` on an `audit:` task is one
module's report. This just folds them into a file a human can read in order,
because 90 findings scattered through a JSONL log is not a review.

    scripts/audit-report.py [blackboard.jsonl] > docs/audit-findings.md

With no argument it reads the local daemon's board.
"""
import json
import os
import sys

HEADER = """# Audit findings

Module-by-module audit of the tree, produced by the fleet: one `audit:<file>`
task per module over ~80 lines, taken and reported by worker agents through the
board. Generated from the blackboard — regenerate with
`scripts/audit-report.py`.

**These are reports, not verdicts.** Each is one agent's reading of one file,
and it has not been triaged. Several were spot-checked by hand; the ones that
held up are fixed and referenced in the git log. Treat the rest as leads: check
the code before acting on any of them.

Findings the fleet raised against its own machinery are the most interesting
ones here, because that code was hours old when it was audited.
"""


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser(
        "~/.local/state/tab-atelier/blackboard.jsonl"
    )
    try:
        lines = open(path).read().splitlines()
    except OSError as e:
        print(f"audit-report: {path}: {e}", file=sys.stderr)
        return 1

    # Last `done` per task wins, matching how the board's own fold resolves it.
    done = {}
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            n = json.loads(raw)
        except ValueError:
            continue  # a torn line from a racing appender is not fatal
        if n.get("kind") == "done" and str(n.get("task", "")).startswith("audit:"):
            done[n["task"]] = (n.get("ok", True), (n.get("msg") or "").strip())

    rows = sorted(done.items())
    if not rows:
        print("audit-report: no audit reports on the board yet", file=sys.stderr)
        return 1

    out = [HEADER, f"*{len(rows)} modules audited.*\n"]
    for task, (ok, msg) in rows:
        module = task[len("audit:"):]
        status = "" if ok else " — **worker reported failure**"
        out.append(f"### `{module}`{status}\n\n{msg or '(no findings reported)'}\n")
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
