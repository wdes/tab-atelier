#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Stage a freshly built APK into a gh-pages tree, ready for the publish step.
#
#     scripts/publish-apk.sh <apk> <version> <short-sha>
#
# Writes the tree to $PUBLISH_SITE_DIR (default ./.publish-site) and prints the
# staged APK path. It does NOT commit or push — the workflow hands the
# directory to peaceiris/actions-gh-pages, the same action the deb publisher
# uses, so both writers produce identical gh-pages commits.
#
# WHY IT FETCHES THE EXISTING SITE FIRST. gh-pages holds the deb, Arch and
# Windows output as well as the APKs. The publish step runs `keep_files: false`
# with `force_orphan: true` (see apt-publish.yml: each publish becomes a fresh
# root commit, so years of 20 MB package blobs cannot pile up in history), which
# means whatever is in the directory we hand over IS the site afterwards.
# Handing over only android/ would therefore wipe every other section. So we
# clone the current gh-pages, replace android/ inside it, and hand back the
# whole tree. That is exactly what apt-publish.yml does for its own sections.
#
# Needs GH_TOKEN (or GITHUB_TOKEN) with contents:write and GITHUB_REPOSITORY.
set -euo pipefail

apk="${1:?usage: $0 <apk> <version> <short-sha>}"
version="${2:?usage: $0 <apk> <version> <short-sha>}"
sha="${3:?usage: $0 <apk> <version> <short-sha>}"

test -s "$apk" || { echo "publish-apk: $apk is missing or empty" >&2; exit 1; }

site="${PUBLISH_SITE_DIR:-.publish-site}"
# Needs a gh-pages remote. In CI that is GITHUB_REPOSITORY plus a token with
# contents:write; PUBLISH_REMOTE_URL overrides it, which is how the staging
# logic below is tested against a local repository.
remote="${PUBLISH_REMOTE_URL:-}"
if [ -z "$remote" ]; then
    repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required, or set PUBLISH_REMOTE_URL}"
    token="${GH_TOKEN:-${GITHUB_TOKEN:?GH_TOKEN or GITHUB_TOKEN is required}}"
    remote="https://x-access-token:${token}@github.com/${repo}.git"
fi

if git ls-remote --heads "$remote" gh-pages | grep -q '\<gh-pages$'; then
    echo "publish-apk: fetching the existing gh-pages tree"
    rm -rf "$site"
    git clone --depth=1 -b gh-pages --single-branch "$remote" "$site"
else
    echo "publish-apk: gh-pages does not exist yet — starting empty"
    rm -rf "$site"
    mkdir -p "$site"
fi

mkdir -p "$site/android"

# Ordering has to come from the NAME, not the mtime. A fresh clone stamps every
# file with the checkout time, so every pre-existing APK looks equally new, and
# force_orphan leaves the branch without history to read either. So the build
# time goes in the filename, fixed-width and UTC, and everything below sorts it
# descending. The name also carries the commit, so an APK on a device can be
# traced back to the source it was built from.
#
# CI passes the name it also gave the build artifact, so the site and the
# artifact carry identical filenames; standalone use composes the same shape.
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
name="${PUBLISH_APK_NAME:-tab-atelier-remote_${stamp}_${sha}_${version}.apk}"
cp "$apk" "$site/android/$name"
echo "publish-apk: staged android/$name ($(du -h "$apk" | cut -f1))"

newest_first() {
    # A glob rather than `ls`: these names are ours and restricted to
    # [A-Za-z0-9_.-], but ls would also word-split them, and `find` would drag
    # in a -printf that is not portable. `[ -e ]` covers the no-match case,
    # where the glob stays literal.
    local f
    for f in "$site"/android/*.apk; do
        [ -e "$f" ] || continue
        printf '%s\n' "$f"
    done | sort -r
}

# Cap: the newest N APKs are kept. They are ~13 MB each and the whole point of
# force_orphan is that the repository does not grow, so an unbounded directory
# would be re-uploaded on every push.
keep=10
newest_first | tail -n +$((keep + 1)) | while IFS= read -r old; do
    echo "publish-apk: dropping $(basename "$old") (keeping the newest $keep)"
    rm -f "$old"
done

# Regenerate the folder index, newest first.
{
    echo '<!DOCTYPE html><meta charset=utf-8><title>Tab Atelier Remote</title>'
    echo '<style>body{font-family:system-ui;max-width:48em;margin:3em auto;padding:0 1em}'
    echo 'code{background:#f4f4f4;padding:.1em .3em;border-radius:3px}'
    echo '.note{background:#fff8d6;border-left:3px solid #d9a900;padding:.6em .9em;margin:1em 0;font-size:.95em}</style>'
    echo '<h1>Tab Atelier Remote</h1>'
    echo '<p>The Android client for Tab Atelier: the workstation&rsquo;s tabs, each opening a real terminal session.</p>'
    echo '<p>Signed release APKs, newest first. Each filename carries its build time, the commit short SHA, and the version.</p>'
    echo '<ul>'
    newest_first | while IFS= read -r f; do
        bn="$(basename "$f")"
        sz="$(du -h "$f" | cut -f1)"
        echo "<li><a href=\"$bn\">$bn</a> &mdash; $sz</li>"
    done
    echo '</ul>'
    echo '<h2>Installing</h2>'
    echo '<p>Download the newest <code>.apk</code> on your Android device and open it. Android asks you to allow installing from this source the first time (Settings &rarr; Apps &rarr; Special access &rarr; Install unknown apps).</p>'
    echo '<div class="note">Self-signed, and not on the Play Store. It is a fork of <a href="https://github.com/connectbot/connectbot">ConnectBot</a> (Apache-2.0) with the backend replaced by Tab Atelier&rsquo;s own API, so it is the same terminal app with a different set of hosts &mdash; yours are the workstation&rsquo;s tabs. Upgrading over an older install keeps working; the signing key has not changed.</div>'
    echo '<p><a href="../">&larr; back to the apt repo</a></p>'
} > "$site/android/index.html"

echo "publish-apk: $site holds $(newest_first | wc -l) APK(s)"
echo "$site/android/$name"
