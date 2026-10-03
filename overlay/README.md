# Overlay — every change we make to ConnectBot

Upstream ConnectBot lives in `../connectbot`, pinned to one commit, never
edited in place. This directory is the complete record of what Tab Atelier
Remote changes about it.

```
patches/   quilt-style patches against upstream files
files/     files that do not exist upstream, copied in verbatim
remove.txt upstream files we delete
```

`../scripts/apply-overlay.sh` applies all three over a pristine checkout, and
`--check` reports whether they still apply after a pin bump without touching
anything.

## Licence position

ConnectBot is Apache-2.0. Distributing a modified build obliges us to:

- ship the licence text — `files/app/src/main/assets/connectbot-LICENSE.txt`,
  packaged into the APK;
- keep upstream's copyright notices, including the per-file header that
  upstream's own Spotless config enforces;
- **say that we changed the files** (§4(b)). The patches in `patches/` are that
  record: each one names the upstream file it modifies and shows the change,
  and each modified region carries a short "Changed for Tab Atelier Remote"
  comment in the code itself so the notice survives being read out of context;
- not use upstream's marks (§6). The app is named and described as Tab Atelier
  Remote, and every user-visible link points at this project, not ConnectBot's.

`../NOTICE` carries the full attribution, including the fork commit.

## Why the overlay is small

The obvious way to fork is to rename the package to `fr.wdes.tab_atelier`
everywhere — about 160 of the ~380 source files — and then edit them. That
would make every upstream sync a rebase across a rewritten tree.

Android does not require it. `applicationId` is independent of the Kotlin
package, so the app is `fr.wdes.tab_atelier` (which the ADI ownership token and
the installed app's upgrade path both require) while the code stays
`org.connectbot` on disk. The overlay then only has to touch the build config,
the strings a user sees, and the files the product actually needs to differ.

## What each patch does

### 0001 — application id, version, NDK

- `applicationId = "fr.wdes.tab_atelier"`. This is the app's identity to
  Google's App Developer Identity registration and to the package manager's
  upgrade path; it must never change.
- Version from Gradle properties instead of upstream's git tags. Upstream
  derives `versionCode`/`versionName` from its own `v*` tags via the
  app-versioning plugin; those tags are not in our history, so the plugin is
  dropped and both values come from `-PappVersionCode`/`-PappVersionName`,
  defaulting to a code above the installed app's `16777472` — Android refuses
  an upgrade that does not increase it.
- `ndkVersion` pinned to an installed NDK. Upstream wants 28.2.13676358. The
  only native code in the app is mosh and the local-shell `exec`, both of which
  the SSH strip removes; until then the pin lets the build run without it.

### 0002 — identity strings

`app_name`, `app_desc` and the help/report links. Upstream's strings name their
project and point at connectbot.org; ours must not (§6). `app_copyright` keeps
Kenny Root's notice and adds ours beside it, rather than replacing it.

## What is deliberately *not* here yet

Milestone 1 keeps every upstream transport compiling, so the app installs and
runs as a working ConnectBot under our package id. Removing SSH, telnet, mosh
and the local shell — their settings, editors, help and strings — is the next
milestone, and it goes into `remove.txt` and further patches as it happens.
