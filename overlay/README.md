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
- `ndkVersion` pinned to an installed NDK. Upstream wants 28.2.13676358, which
  is not on every build machine; 26.1.10909125 is, and builds. The app's only
  native code is mosh's JNI and the local-shell `exec`, both of which stay —
  the app remains a full SSH client, and the tab-atelier type is added beside
  it rather than replacing anything.
- `BuildConfig.BUILD_COMMIT` from `-PbuildCommit`, defaulting to `"unknown"`.
  `scripts/build-apk.sh` passes it — CI the short SHA it computed for the APK
  filename, a local build `git rev-parse --short HEAD` — so the About screen
  names the commit the installed APK was built from.

### 0002 — identity strings

`app_name`, `app_desc` and the help/report links. Upstream's strings name their
project and point at connectbot.org; ours must not (§6). `app_copyright` keeps
Kenny Root's notice and adds ours beside it, rather than replacing it.

This patch rewrites upstream strings and **nothing else**. Strings we *add* do
not belong here — see "Adding a string" below.

### Adding a string (no patch)

Strings this fork adds go in
`files/app/src/main/res/values/strings_tabatelier.xml`. That file is copied in
verbatim, so adding a string is adding a line to it, and an upstream sync has
nothing to rebase. Android's resource merger combines every file under
`res/values/`, so a resource does not care which file declares it.

**Replacing** an upstream string is the case that cannot work this way: two
files in the same source set declaring the same name is a duplicate-resource
error, not an override. That is why the few upstream strings we rewrite stay in
patch 0002 next to their originals, and why that patch only changes when the
rebranding does.

The distinction is worth keeping. Every string added through a patch turns a
one-line change into a merge conflict waiting for the next pin bump; every
string kept out of one costs nothing.

### 0003 — About: credit and build commit

`HelpScreen.kt`'s About section. It shows `ConnectBot & Tab-Atelier`, naming
upstream and this fork on one line, and both commits under the version — the
Tab Atelier commit and the upstream ConnectBot commit it was built on, named
separately so a bug report can say which half is at fault. The two community
buttons that belong to upstream (their IRC/mailing list, their mosh release)
are commented out with their reasons beside them, and two source-code buttons
replace them. The labels are literals rather than resources on purpose: they are
the same in every locale, and a translated commit hash or URL would be a defect.

### 0004 — the tab-atelier server type

A fifth protocol, `tabatelier`, beside ssh/telnet/mosh/local — added, not
substituted, so every upstream transport keeps working. A host of this type is a
tab-atelier daemon: one URL addresses it, a bearer token authenticates against
it, and its row in the host list expands into that daemon's tabs, newest-used
first.

Files: `transport/Transport.kt`, `transport/AbsTransport.kt`,
`transport/TransportFactory.kt`, `data/entity/Host.kt`,
`data/ConnectBotDatabase.kt`, `HostRepository.kt`, `TerminalBridge.kt`,
`TerminalManager.kt`, the host list and host editor screens and their
ViewModels, `AndroidManifest.xml`, and the build files (OkHttp, which the app
did not previously depend on).

New files live in `files/`, since upstream has nothing like them:
`tabatelier/TabAtelierClient.kt` (the parsed base URL, HTTP, TLS),
`tabatelier/TabAtelierTab.kt` (the model and its parsing),
`transport/TabAtelier.kt` (the transport), and
`app/schemas/org.connectbot.data.ConnectBotDatabase/12.json` (the Room schema
for version 12, which upstream's committed `app/schemas/` does not have yet —
`exportSchema` is on, and an `AutoMigration` is validated against it).

Three decisions worth not re-litigating:

- **The URL is a Room column, and that is a migration.** `tabatelier_url`
  (nullable, so the migration is a plain `AutoMigration` from 11 to 12) holds
  `http://host:7890`, `https://host` or `https://host:8443/prefix`. A scheme and
  a path prefix have nowhere else to live — `hostname`/`port` cannot carry
  either, and the token must not move: it is a per-host secret and belongs in
  the Keystore-backed store upstream already has (`util/SecurePasswordStorage.kt`,
  keyed `password_<hostId>`), out of exports and backups. `hostname` and `port`
  are kept in step with the URL so the shortcut intent and anything else that
  still reads them keeps working.
- **`http` is a real choice, not a fallback.** The daemon can serve plain HTTP
  (`start_api_server`, distinct from `start_api_server_tls`), which is what a
  trusted LAN or a tunnel wants. An `http://` base is built with no TLS
  configuration at all — no pin, no trust manager, no hostname verifier — and is
  never silently upgraded to https.
- **TLS is trust on first use, and the gate is a trust manager.** The daemon's
  certificate is self-signed or a Cloudflare Origin certificate, so neither the
  system CA store nor name validation can be the gate. The **root cause of the
  first release's "self-signed certificates do not work"** is worth stating
  plainly, because the wrong fix is the obvious one: a `hostnameVerifier` alone
  never runs, since OkHttp's *default* trust manager rejects the chain during
  the handshake, before any verifier is consulted. So an `X509TrustManager`
  decides, on the peer's Subject Public Key Info: first use of a host is
  accepted so the request can be made at all, the key is *persisted* only once a
  response to that request has come back, and every later connection must
  reproduce it — including a changed key, which fails during the handshake with
  a message naming the host. The `hostnameVerifier` (which compares the pin) and
  OkHttp's `CertificatePinner` stay as independent second checks; the pin string
  is whatever `CertificatePinner.pin(cert)` produces, so a recorded pin cannot
  fail to verify against itself.

**A server is re-probed when what the probe depends on changes.** The per-host
tab state is keyed by host id and invalidated when the host's URL changes or,
for better or worse, its token appears or disappears — which is how a server
someone just fixed in the editor stops showing the error the old address earned
them. It is deliberately *not* re-probed on every hosts-Flow emission:
ConnectBot writes `Host.lastConnect` on every connect, so "reload when the row
changes" would hit every daemon each time a session starts.

**The app names itself** `ta-remote/<version> (Android)` on every request, an
OkHttp interceptor so stage 2's WebSocket carries it too. That is for the
daemon's logs — the daemon does not branch on it — and it matches the retired
Slint client's string for continuity.

**Tapping a tab does nothing yet.** The WebSocket terminal is the next stage;
until it lands, the row says so rather than failing silently.

## Current state

The app is a working ConnectBot under our package id, plus the tab-atelier type:
servers can be added, their tabs are listed and ordered by last use, and a tab
opens no session yet. Nothing upstream is removed — SSH, telnet, mosh and the
local shell all still work, and `remove.txt` is empty. A future milestone may
retire the transports that have no use here, but only if that is wanted: the
additive shape is what keeps an upstream sync to a pin bump.

