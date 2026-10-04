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
- **TLS is validated by the device where the device can, and pinned where
  nothing else can identify the server.** The daemon's own certificate is
  self-signed, so neither the system CA store nor name validation can be the
  gate for it — but a certificate a CA *did* sign must be validated by that CA,
  and pinning it would be wrong: such a certificate is renewed as a matter of
  course, and a client that refused the renewed one would be broken by design.
  Which of the two a server is is the device's judgement, not ours, and the
  trust manager asks the device directly (`TrustManagerFactory` over the
  default store) rather than keeping a second opinion:

  - **The device validates the chain** — a public CA signed it, or the user
    installed its CA (a Cloudflare Origin certificate with its CA installed,
    which `AGENTS.md` documents as a supported deployment). Nothing is pinned,
    and any pin left over from before is *dropped*, because a pin that outlived
    its reason would refuse the renewal the device has just accepted. The
    device's own hostname verifier checks the name.
  - **The device cannot** — the daemon's self-signed certificate, or an origin
    certificate whose CA this device does not have. Nothing else identifies the
    server, so its key is its identity: trust on first use, then require that
    exact key. This is the case a pin belongs to, and the name is deliberately
    *not* checked there: a self-signed certificate carries the names the daemon
    knows itself by, and the address a user typed (a LAN address, `127.0.0.1`)
    is often not one of them. The key has just been required instead.

  The **root cause of the first release's "self-signed certificates do not
  work"** is worth stating plainly, because the wrong fix is the obvious one: a
  `hostnameVerifier` alone never runs, since OkHttp's *default* trust manager
  rejects the chain during the handshake, before any verifier is consulted. So
  an `X509TrustManager` decides, on the peer's Subject Public Key Info: first
  use of a server is accepted so the request can be made at all, the key is
  *persisted* only once a response to that request has come back, and every
  later connection must reproduce it — including a changed key, which fails
  during the handshake with a message naming the host. The `hostnameVerifier`
  follows the same decision (the device's verifier, or the pin), and OkHttp's
  `CertificatePinner` is added for a pinned server only. A pin string is
  whatever `CertificatePinner.pin(cert)` produces, so a recorded pin cannot
  fail to verify against itself.

  Two details of that arrangement are load-bearing. The verifier answers *true*
  when nothing is pinned, because OkHttp enforces a verifier's answer and
  answering false there would refuse the very connection whose response records
  the pin — circular, and the same bug reported as "hostname not verified"
  instead of as a certificate error. And a pin is recorded from the key the
  trust manager accepted, never from `Response.handshake`, which is not always
  populated — when it was the only source, TOFU quietly became "trust
  everything, forever".

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

**Tapping a tab opens it.** The session is the daemon's own WebSocket protocol,
not SSH: one tag byte then the payload, with keystrokes out, output in (gzipped
output inflated with `GZIPInputStream` — gzip, not a raw deflate stream), the
terminal size reported, and a "focused" frame as the session opens, which is what
makes the daemon stamp the tab's `last_used_at`. So using a tab is what moves it
up the list the list sorts by.

Two details worth not rediscovering:

- **Resize is a no-op in the daemon's v1.** A tab is a terminal on a real screen
  and the workstation wins, so a phone renders a tab's output at the
  workstation's width, with the workstation's wrapping. The frame is sent anyway:
  it costs nothing and works the day the daemon honours it.
- **The size frame is JSON**, `{"cols":N,"rows":M}`, not the two big-endian
  shorts the tag suggests.

Which tab to open cannot ride on the host row — a row is the daemon, not one of
its tabs, and a WebSocket cannot be asked for a tab once it is open. So the id
travels from the tap through the console route's optional `?tab=` argument and is
*consumed* by the transport (taken, not read), so a later session cannot inherit
the previous one's tab.

**A row's trailing content is a stack, not a sequence.** The chevron that shows
and hides a daemon's tab list sits beside the overflow button, and putting both
in the trailing slot as siblings does not lay them out side by side — they land
on the same coordinates, and the one drawn last takes every tap. Measured before
the wrapper existed, both at `Rect.fromLTRB(1856.0, 192.0, 1936.0, 272.0)`:
`performClick` on the chevron reached the overflow button, so the tab list could
not be hidden by its own control. Upstream never hit this because its trailing
slot has only ever held one child. They are wrapped in a `Row`, and the
regression test asserts the *click* rather than the presence of the node — a
stacked control is present, visible and enabled, and still unusable.

### 0005 — leaving a terminal puts the keyboard away

One file, `ui/screens/console/ConsoleScreen.kt`, deliberately its own patch
rather than folded into 0004: it is not about the tab-atelier server type, it
applies to every transport, and upstream may well want it.

The app is a single Activity, so the IME outlives the console screen: backing out
to the host list left the keyboard up over a list with no text field in it. The
fix hides it imperatively from a `DisposableEffect` on dispose. Clearing
`showSoftwareKeyboard` would not do — disposal is the last thing the composition
does, so there is no recomposition left to act on a state change.

It is guarded on `isChangingConfigurations`, so rotating the device mid-session
does not close the keyboard: MainActivity does not handle orientation itself, so
a rotation disposes this screen without the user having left anything.

## Current state

The app is a working ConnectBot under our package id, plus the tab-atelier type:
servers can be added, their tabs are listed and ordered by last use, and tapping
one opens that tab's terminal. Nothing upstream is removed — SSH, telnet, mosh
and the local shell all still work, and `remove.txt` is empty. A future milestone
may retire the transports that have no use here, but only if that is wanted: the
additive shape is what keeps an upstream sync to a pin bump.

The session path has now been run against a real daemon, which is how two bugs
were found that nothing else would have caught:

- **The transport never told the bridge it was connected.** `bridge.onConnected()`
  is what creates the Relay, which is what reads the transport; every other
  transport calls it, `TabAtelier` did not, so the terminal sat on "connecting
  via tabatelier…" forever. `connect()` now waits for the WebSocket to open and
  then signals it, which is also why it may block — it runs on the io dispatcher.
- **The daemon refuses the token as an `Authorization` header on a WebSocket
  upgrade** and accepts it only as `?token=`. This is a daemon-side bug:
  `extract_token` (`src/api_ws.rs`) reads the query first and the header second,
  but the query lookup is `req.uri().query()?`, whose `?` returns from the whole
  function when there is no query — so a bare `/tabs/by-id/{id}/ws` never reaches
  the documented header fallback and 401s. Verified by raw handshake: header
  alone → 401, `?token=` → 101. The app uses the query form (as the daemon's own
  browser client does); fixing `extract_token` would let the header form work and
  let the client drop `?token=`, which is the better primitive since it keeps the
  token out of URLs and logs.

Not yet exercised on a real device: everything above is measured from the host
against a live daemon over the app's own code paths, not from the phone.

