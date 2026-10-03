# Tab Atelier Remote — Android app

The Android client for [Tab Atelier](../README.md): a list of the workstation's
tabs, each opening a real terminal session against the daemon.

It is a **fork of [ConnectBot](https://github.com/connectbot/connectbot)**, and
this branch is shaped so that stays obvious and maintainable.

## Layout

| Path | What it is |
| --- | --- |
| `connectbot/` | Upstream ConnectBot, a git **submodule** pinned to one commit. Read-only: never edited here, never pushed to. |
| `overlay/` | **Everything we change**, one file per upstream file, laid over the submodule before the build. |
| `scripts/build-apk.sh` | Applies the overlay, then builds and signs the APK with Gradle. |
| `LICENSE`, `NOTICE` | Apache-2.0, plus the attribution the fork owes upstream. |

There is no `keystore/` directory: the signing key and its password are
deliberately outside this repository — see "Signing".

## Why a submodule plus an overlay

The alternative — vendoring a copy of ConnectBot and editing it — makes an
upstream change a manual diff against a tree we rewrote. Here, upstream keeps
its own history at its own path, and a sync is two steps: bump the pin, fix up
the overlay. Nothing is ever merged across a renamed tree.

The overlay stays small on purpose. The Android `applicationId` is independent
of the Kotlin package name, so we take `fr.wdes.tab_atelier` (required by the
ADI ownership token and by upgrading the app that is already installed) while
the code underneath remains `org.connectbot`. Renaming ~160 files would have
made every upstream sync a rebase, for no user-visible gain.

Per section 4(b) of the Apache License, Version 2.0, every file in `overlay/`
is a modified ConnectBot source and its header says so.

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
Android NDK only if a native module survives — see the overlay, which removes
the two that did (mosh and the local-shell exec).

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
  script asks GitHub for the push diff instead, and treats the submodule pin
  (`connectbot`) and `overlay/` as the inputs that reach the APK.
- **The version** is `0.6.<run number>` with versionCode
  `16777472 + run number`. The installed app is 16777472 and Android refuses an
  upgrade that does not increase it, so the run number is what keeps it
  monotonic without a release process.
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
git -C connectbot fetch origin && git -C connectbot checkout <new commit>
scripts/apply-overlay.sh --check   # does the overlay still apply?
```

Then commit the new pin. When upstream touches a file the overlay also owns,
`--check` names the conflicting hunks rather than leaving it to the compiler.
