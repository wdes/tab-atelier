#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Transform `overlay/patches/*.patch` ⇄ git commits, the way `gbp pq` does.

`gbp pq` itself cannot run here. It is built for a Debian source package, and this
is a ConnectBot fork, so it stops at the missing `debian/control` and wants a
`debian/changelog` to derive a version from for naming tags. None of that is what
does the work, though — the work is `gbp.patch_series` (reading the series and
parsing patch files) and `gbp.scripts.common.pq` (`apply_and_commit_patch` and
`format_patch`). This script calls those same functions, so the transformation is
gbp's; only the Debian packaging around it is omitted.

    pq.py import    patches → commits on patch-queue/<branch>
    pq.py export    commits → patches

Why this exists: it is how the overlay stops being able to lose work. A patch
lives as a commit on a branch, so there is no uncommitted state in the submodule
to be discarded by a stray `checkout`, no patch to forget to regenerate, and a
hash-based reproduction check is not the only thing standing between an edit and
oblivion. Editing a patch becomes amending a commit and exporting.

The patches this can read are plain `git diff` files as well as `git format-patch`
ones: gbp takes the subject from the filename when a patch carries no mail
headers, and uses the fallback author below when it carries no authorship. `import`
records each patch's original name in a `Gbp-Pq: Name` trailer, so `export` gives
the same filenames back rather than re-deriving them from commit subjects.
"""

import argparse
import os
import re
import shutil
import sys
import tempfile

import gbp.log
from gbp.git import GitRepository, GitRepositoryError
from gbp.patch_series import PatchSeries
from gbp.scripts.common.pq import (
    apply_and_commit_patch,
    format_patch,
    is_pq_branch,
    pq_branch_base,
    pq_branch_name,
)

HERE = os.path.dirname(os.path.abspath(__file__))
APP = os.path.dirname(HERE)
# The upstream ConnectBot checkout, a read-only submodule of the app repo.
REPO = os.path.join(APP, "connectbot")
PATCH_DIR = os.path.join(APP, "overlay", "patches")
SERIES = os.path.join(PATCH_DIR, "series")

# How a patch filename is numbered, gbp's own default: 0001-foo.patch
PREFIX = re.compile(r"^\d{4}-")

FALLBACK_AUTHOR = {"name": "William Desportes", "email": "williamdes@wdes.fr", "date": None}


def repo_or_die():
    try:
        repo = GitRepository(REPO)
    except GitRepositoryError as e:
        sys.exit("Not a git repository: %s (%s)" % (REPO, e))
    return repo


def read_series():
    if not os.path.exists(SERIES):
        sys.exit(
            "No series file at %s — `export` writes one, or write it by hand "
            "listing one patch filename per line." % SERIES
        )
    queue = PatchSeries.read_series_file(SERIES)
    if not queue:
        sys.exit("The series at %s lists no patches." % SERIES)
    return queue


def do_import(args):
    repo = repo_or_die()
    branch = repo.branch
    pq_branch = pq_branch_name(branch)

    if is_pq_branch(branch):
        # Re-importing from the queue is the normal case once a commit has been
        # edited, and it must replay onto the same upstream base the queue already
        # sits on — never onto the queue's own head, which has every patch applied
        # and so would apply each one on top of itself.
        base = repo.rev_parse(pq_branch_base(branch))
        gbp.log.info("Re-importing, replacing this queue (base %s)" % base[:7])
        # Rewind the queue to upstream; the patches are about to be replayed over
        # it. Any commit added by hand on the queue is replaced by this, which is
        # the point: the patches are the record, and the queue is regenerated.
        repo.force_head(base, hard=True)
    else:
        base = repo.head
        if repo.has_branch(pq_branch):
            gbp.log.info("Replacing the existing %s (was %s)" % (pq_branch, repo.rev_parse(pq_branch)[:7]))
            repo.delete_branch(pq_branch)
        repo.create_branch(pq_branch, base)
        repo.set_branch(pq_branch)

    queue = read_series()
    gbp.log.info("Importing %d patches onto %s (base %s)" % (len(queue), pq_branch, base[:7]))
    for patch in queue:
        # gbp's subject for a header-less patch is derived from the filename, and
        # the name is recorded so export can hand the same one back.
        name = os.path.basename(patch.path)
        name = PREFIX.sub("", name)
        name = re.sub(r"\.patch$", "", name)
        gbp.log.info("  %s" % os.path.basename(patch.path))
        apply_and_commit_patch(repo, patch, FALLBACK_AUTHOR, name=name)

    gbp.log.info("%d commits on %s" % (len(queue), pq_branch))
    return 0


def do_export(args):
    repo = repo_or_die()
    branch = repo.branch
    if not is_pq_branch(branch):
        sys.exit(
            "On %s, which is not a patch queue. Run `import` first (it creates "
            "%s)." % (branch, pq_branch_name(branch))
        )

    base_branch = pq_branch_base(branch)
    base = repo.rev_parse(base_branch)
    commits = list(repo.get_commits(base, repo.head))
    if not commits:
        sys.exit("%s has no commits over %s — nothing to export." % (branch, base_branch))

    # Oldest first: that is the series order, and gbp numbers as it writes.
    commits.reverse()

    # Everything is written to a temporary directory and only moved into place
    # once every patch has been produced. The first version of this deleted the
    # old patches first and then wrote the new ones, so a failure part-way — and
    # there was one, `core.abbrev=0` below — left the overlay with no patches at
    # all. Regenerating must not be able to destroy the thing it regenerates.
    staging = tempfile.mkdtemp(prefix="pq-export-")
    try:
        gbp.log.info("Exporting %d commits" % len(commits))
        series = []
        for rev in commits:
            info = repo.get_commit_info(rev)
            # A name carried from import, so the filename is the one the patch had
            # rather than one re-derived from the subject. gbp parses this trailer
            # itself in `gbp pq export`; doing it here keeps the behaviour identical.
            name = None
            for line in (info.get("body") or "").split("\n"):
                m = re.match(r"Gbp-Pq: Name (.*)$", line)
                if m:
                    name = m.group(1).strip()
            # gbp splits the name into base and *suffix*, and writes
            # `num_prefix + base + suffix` — so a name without an extension
            # produces a patch file with no `.patch` on the end. That matters more
            # than it looks: apply-overlay.sh globs `*.patch`, so a patch named
            # without the extension is one the overlay never sees. The extension is
            # carried here rather than in the trailer, so the trailer stays a name.
            if name and not name.endswith(".patch"):
                name = name + ".patch"
            # abbrev=None, and it is not cosmetic. gbp formats this into
            # `-c core.abbrev=%d`, so the obvious `False` becomes 0 and git refuses
            # it outright ("abbrev length out of range: 0"). None is what gbp's own
            # export passes when no abbreviation length is configured.
            #
            # renumber=True, because passing a name switches gbp's numbering OFF:
            # with renumber=False it clears the number prefix and the file lands as
            # `application-id-and-version.patch`, losing the 0001- the series
            # relies on. renumber makes gbp write the prefix from the position in
            # the series, which is what those numbers are for.
            format_patch(
                outdir=staging,
                repo=repo,
                commit_info=info,
                series=series,
                abbrev=None,
                name=name,
                renumber=True,
            )
            # gbp appends the *filename* to series, not an object, so this is the
            # path the patch was written to.
            gbp.log.info("  %s" % os.path.basename(series[-1]))

        # Now, and only now, replace what is on disk. Numbered patch files first:
        # a patch removed on the branch must not linger as a file nothing lists.
        # Matched by prefix rather than by extension, because a patch written by an
        # earlier version of this script can be missing its `.patch` — and leaving
        # one behind would put a second copy of the same change in the series.
        for entry in sorted(os.listdir(PATCH_DIR)):
            if PREFIX.match(entry) and os.path.isfile(os.path.join(PATCH_DIR, entry)):
                os.remove(os.path.join(PATCH_DIR, entry))
        for patch in series:
            shutil.move(patch, os.path.join(PATCH_DIR, os.path.basename(patch)))

        with open(SERIES, "w") as f:
            for patch in series:
                f.write("%s\n" % os.path.basename(patch))
    finally:
        shutil.rmtree(staging, ignore_errors=True)

    gbp.log.info("Wrote %d patches and %s" % (len(series), os.path.basename(SERIES)))
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("import", help="overlay/patches/*.patch -> commits on patch-queue/<branch>")
    sub.add_parser("export", help="commits on patch-queue/<branch> -> overlay/patches/*.patch")
    args = parser.parse_args(argv)

    gbp.log.setup(color=True, verbose=False)
    return {"import": do_import, "export": do_export}[args.command](args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
