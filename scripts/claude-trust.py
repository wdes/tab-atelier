#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Pre-trust folders for Claude Code, so a fresh agent tab does not stop to ask.

    scripts/claude-trust.py /mnt/Dev/@wdes /mnt/clients        # whole trees
    scripts/claude-trust.py --list                             # what is trusted
    scripts/claude-trust.py --dry-run /mnt/clients/ACME

Claude Code has no global "trust everything" setting: trust is recorded per
project in ~/.claude.json as `projects["<dir>"].hasTrustDialogAccepted`. That
is the only lever, so this writes exactly that key, for directories you name.

A directory argument trusts THAT directory and, with --depth, its immediate
children — a client tree where every repo is its own project would otherwise
need one prompt each, which is the case that actually hurts when a fleet is
spawning tabs.

WHAT THIS IS: skipping a confirmation for code you already own. It does not
grant permissions, does not enable --dangerously-skip-permissions, and does
not touch tool allowlists. Trusting a directory you did not write is exactly
the thing the prompt exists for; do not point this at a downloads folder.

CONCURRENCY: Claude rewrites ~/.claude.json on exit, so a session running now
can overwrite this. Run it when the tabs are idle, or re-run it after. The file
is written via a temp + rename, and a .bak is kept.
"""
import argparse
import json
import os
import shutil
import sys
import tempfile

CONFIG = os.path.expanduser("~/.claude.json")
KEY = "hasTrustDialogAccepted"


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}
    except ValueError as e:
        sys.exit(f"claude-trust: {path} is not valid JSON ({e}) — refusing to rewrite it")


def targets(dirs, depth):
    """The directories to trust: each argument, plus its children at --depth 1."""
    out = []
    for d in dirs:
        real = os.path.realpath(os.path.expanduser(d))
        if not os.path.isdir(real):
            print(f"  skip (not a directory): {d}", file=sys.stderr)
            continue
        out.append(real)
        if depth >= 1:
            for child in sorted(os.listdir(real)):
                full = os.path.join(real, child)
                # Only real directories, and not the dot-dirs — a fleet does
                # not open tabs in .git.
                if os.path.isdir(full) and not child.startswith("."):
                    out.append(full)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dirs", nargs="*", help="directories to trust")
    ap.add_argument("--depth", type=int, default=1, help="also trust immediate children (default 1, 0 to disable)")
    ap.add_argument("--dry-run", action="store_true", help="report what would change")
    ap.add_argument("--list", action="store_true", help="list trusted projects and exit")
    args = ap.parse_args()

    cfg = load(CONFIG)
    projects = cfg.setdefault("projects", {})

    if args.list:
        trusted = sorted(k for k, v in projects.items() if isinstance(v, dict) and v.get(KEY))
        for t in trusted:
            print(t)
        print(f"\n{len(trusted)} trusted of {len(projects)} known", file=sys.stderr)
        return 0
    if not args.dirs:
        ap.error("name at least one directory (or --list)")

    wanted = targets(args.dirs, args.depth)
    added, already = [], 0
    for d in wanted:
        entry = projects.get(d)
        if isinstance(entry, dict) and entry.get(KEY):
            already += 1
            continue
        added.append(d)
        if not args.dry_run:
            # Preserve everything else Claude keeps per project (history,
            # onboarding flags); only add the one key.
            projects.setdefault(d, {} if not isinstance(entry, dict) else entry)[KEY] = True

    for d in added:
        print(f"  {'would trust' if args.dry_run else 'trusted'}: {d}")
    print(f"{len(added)} added, {already} already trusted, {len(wanted)} considered", file=sys.stderr)

    if args.dry_run or not added:
        return 0

    shutil.copy(CONFIG, CONFIG + ".bak") if os.path.exists(CONFIG) else None
    # Temp + rename: ~/.claude.json holds every session's history, and a
    # half-written one loses all of it.
    d = os.path.dirname(CONFIG) or "."
    with tempfile.NamedTemporaryFile("w", dir=d, delete=False) as tmp:
        json.dump(cfg, tmp, indent=2)
        tmp.flush()
        os.fsync(tmp.fileno())
        temp_name = tmp.name
    os.replace(temp_name, CONFIG)
    print("note: a Claude session running now rewrites this file on exit and can undo it", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
