# Tab Atelier Remote — Android app

The Android client for [Tab Atelier](../README.md): a list of the workstation's
tabs, each opening a real terminal session against the daemon.

It is a **fork of [ConnectBot](https://github.com/connectbot/connectbot)**, and
this branch is shaped so that stays obvious and maintainable.

## Layout

| Path | What it is |
| --- | --- |
| `connectbot/` | ConnectBot's tree with our changes on top, as a **squashed `git subtree`**. Our changes are ordinary commits here. |
| `docs/connectbot-fork.md` | What we changed and why — the record the Apache-2.0 §4(b) notice points at. |
| `wip/` | Unfinished work kept as patches, inert. See its own note below. |
| `scripts/build-apk.sh` | Builds and signs the APK with Gradle. |
| `LICENSE`, `NOTICE` | Apache-2.0, plus the attribution the fork owes upstream. |

There is no `keystore/` directory: the signing key and its password are
deliberately outside this repository — see "Signing".

## Why a subtree

It was a **submodule** pinned to upstream, with our changes as quilt-style
patches in `overlay/`, applied before each build. That worked, but one property
could not be designed around: *an applied patch set is uncommitted changes by
definition*, so the submodule's working tree was permanently dirty, and a clean
tree was only reachable by committing our change into a repository whose remote
is upstream's — a commit that could never be pushed, and that this repository's
gitlink could not name without pointing at a commit nobody else has.

So `connectbot/` is now a **squashed subtree**: upstream's *tree*, without
upstream's 20 MB of history. Our changes are ordinary commits, the working tree
is clean, and `git add -A` cannot record a pointer to a commit that exists only
locally. A change to the app is a commit that touches `connectbot/…`.

Upstream's code stays at its own path and is never merged across a renamed tree,
so the reason for the old shape still holds. What a sync looks like:

```sh
# Fetch upstream into the subtree, squashed again. `git subtree` on this
# machine cannot do the add/pull itself (see docs/connectbot-fork.md), so the
# same thing by hand is: read-tree the new upstream tree over connectbot/,
# commit it as a subtree merge with the usual trailers, then replay our
# commits over it and resolve.
git subtree pull --prefix=connectbot https://github.com/connectbot/connectbot.git main --squash
```

To see exactly what we changed, diff against the recorded upstream base — the
`git-subtree-split` trailer of the squash commit, which
`scripts/build-apk.sh` reads for the About screen:

```sh
base=$(git log -1 --grep='git-subtree-split:' --format='%(trailers:key=git-subtree-split,valueonly)')
git diff "$base" -- connectbot/
```

The change set stays small on purpose. The Android `applicationId` is independent
of the Kotlin package name, so we take `fr.wdes.tab_atelier` (required by the
ADI ownership token and by upgrading the app that is already installed) while
the code underneath remains `org.connectbot`. Renaming ~160 files would have
made every upstream sync a rebase, for no user-visible gain.

Per section 4(b) of the Apache License, Version 2.0, every ConnectBot source
file we modified says so in its header.

## Signing

The release APK is signed under the alias `ta-remote`, certificate SHA-256
`D8:EE:6B:25:59:9F:1D:73:12:5F:F6:CD:1D:55:58:15:D8:64:CA:33:B8:6C:22:64:2E:0B:A6:BD:AA:52:20:CA`.

**Neither the keystore nor its password is in this repository.** The repository
is public, and that certificate is the app's identity: Google's App Developer
Identity registration and the installed app's upgrade path both key on it. A
leaked key lets anyone sign a build that installs *as an upgrade* over the real
app on any device that has it, and a different key would mean a fresh install
for every user and a lost ownership claim.

Both live with the other persistent identity material under
`~/.config/tab-atelier/`, which is where `scripts/build-apk.sh` reads them from:

| File | Contents |
| --- | --- |
| `~/.config/tab-atelier/release.keystore` | the key (mode 0600) |
| `~/.config/tab-atelier/keystore.properties` | `storePassword=` and `keyAlias=` (mode 0600) |

`ANDROID_KEYSTORE_FILE` and `ANDROID_KEYSTORE_CONFIG` point the build elsewhere
if needed.

In CI both arrive as **environment secrets on the `android-app` environment**
and are handed to the build as environment variables, so nothing is written
into the checkout:

| Secret | Required | Notes |
| --- | --- | --- |
| `ANDROID_KEYSTORE_BASE64` | yes | the keystore, base64-encoded; already exists for main's Android workflow |
| `ANDROID_KEYSTORE_PASSWORD` | yes | the store **and** key password — main never needed it because it reads the password from the repository |
| `ANDROID_KEYSTORE_ALIAS` | no | defaults to `ta-remote` |

The workflow fails with a named error if a required secret is missing, rather
than letting Gradle fail obscurely later.

To rotate the key you would have to accept a fresh install for every user; to
rotate only the *password*, `keytool -storepasswd`/`-keypasswd` keep the same
certificate, so the identity and the upgrade path are unaffected.

## Building

```sh
scripts/build-apk.sh            # signed release APK
scripts/build-apk.sh --debug    # debug APK, upstream's own debug key
```

Needs `ANDROID_HOME` (defaults to this machine's SDK), a JDK 21, and an
Android NDK only if a native module survives. We removed the two that did —
mosh and the local-shell exec — which is a commit deleting them like any other,
and it is why `ndkVersion` is pinned in `connectbot/app/build.gradle.kts`.

## CI and publishing

`.github/workflows/android-apk.yml` runs on pushes to this branch (and on
demand) and does two things: builds the signed release APK, and publishes it to
the website.

- **Signing** comes from the `android-app` environment, the same one main's
  Android workflow uses. Both `ANDROID_KEYSTORE_BASE64` and
  `ANDROID_KEYSTORE_PASSWORD` must be set there; the job checks for them up
  front and names whichever is missing. See "Signing" above.
- **The gate** is `scripts/apk-needed.sh`, not `on.push.paths`. A path filter
  is evaluated against the commits *in a push*, and a tag points at an
  already-pushed commit, so a `paths:` filter silently skips tag builds. The
  script asks GitHub for the push diff instead, and treats `connectbot/` and the
  build scripts as the inputs that reach the APK.
- **The version** is the tag for a `v*` tag build, otherwise `0.6.<run number>`,
  with versionCode `16777472 + run number` either way. The installed app is
  16777472 and Android refuses an upgrade that does not increase it, so the run
  number is what keeps it monotonic without a release process.
- **The APK's filename** is `tab-atelier-remote_<utc>_<sha>_<version>.apk`, and
  the build artifact carries that same name: `apt-publish.yml` fetches the
  newest android-apk artifact and uses its name as the destination filename, so
  the two agree while both publishers are in place.
- **Publishing** writes only `android/` on gh-pages, keeping the newest 10
  APKs next to an `index.html`. gh-pages also serves the deb, Arch and Windows
  output, so `scripts/publish-apk.sh` fetches the current tree first and hands
  the whole thing to `peaceiris/actions-gh-pages` — the same action
  `apt-publish.yml` uses. Both carry the same `gh-pages` concurrency group,
  because both rebuild the branch from a clone and would otherwise undo each
  other.
- **Ordering is by filename**, not mtime: a fresh clone stamps every file with
  the checkout time, and `force_orphan` leaves no history, so the build time is
  in the name (`tab-atelier-remote_<utc>_<sha>_<version>.apk`).

APKs land at <https://deb.tab-atelier.wdes.eu/android/>.

## Syncing upstream

```sh
git subtree pull --prefix=connectbot https://github.com/connectbot/connectbot.git main --squash
```

Then replay our commits over the new upstream tree and resolve what conflicts —
the tree is squashed, so git reports those as ordinary conflicts on ordinary
paths, and the compiler finds the rest: a file we deleted that upstream
resurrected, a symbol we changed that upstream moved. The same base trailer as
above says what we changed, so the conflict set is exactly `git diff "$base" --
connectbot/` and no more.

`git subtree` cannot do the add/pull on this machine — this git version rejects
the `read-tree --prefix=` it runs without a trailing slash — so the same
operation by hand is: `git read-tree --prefix=connectbot/ <new upstream tree>`
merged as a subtree commit, then replay. `docs/connectbot-fork.md` has the
detail.
