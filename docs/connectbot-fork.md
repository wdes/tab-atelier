# The ConnectBot fork — every change we make

ConnectBot's code lives in `../connectbot` as a **squashed `git subtree`**: its
tree, without its history. Our changes are ordinary commits that touch it, so
this document and `git diff` are the complete record of what Tab Atelier Remote
changes about it.

To see exactly that diff, against the upstream base the squash records:

```sh
base=$(git log -1 --grep='git-subtree-split:' --format='%(trailers:key=git-subtree-split,valueonly)')
git diff "$base" -- connectbot/
```

That command and the app's About screen read the same trailer, so both need
**history**: the squash commit is forty-odd back, and a shallow clone — `--depth 1`,
which is what `actions/checkout` does unless told otherwise — reaches neither.
The APK workflow asks for full history for exactly this reason, and
`scripts/build-apk.sh` warns when it cannot find the trailer rather than leaving
the About screen reading "unknown" with nothing to explain it.

A commit that changes `connectbot/` is a change to the app; a commit elsewhere is
about the build, the docs, or this record.

**This was not always the shape.** The code was a submodule pinned to upstream,
with our changes kept as quilt-style patches in `overlay/` (`patches/`,
`files/`, `remove.txt`) and applied over a pristine checkout by
`scripts/apply-overlay.sh` before every build. That has one property that cannot
be designed around — *an applied patch set is uncommitted changes by definition*
— so the working tree was permanently dirty, and a clean tree was only reachable
by committing into a repository whose remote is upstream's. A squash commit's
`git-subtree-split` trailer is what replaced the pin, and it survives future
syncs with nothing to remember.

## Licence position

ConnectBot is Apache-2.0. Distributing a modified build obliges us to:

- ship the licence text — `connectbot/app/src/main/assets/connectbot-LICENSE.txt`,
  packaged into the APK;
- keep upstream's copyright notices, including the per-file header that
  upstream's own Spotless config enforces;
- **say that we changed the files** (§4(b)). The commits that touch `connectbot/`
  are that record — each names the upstream file it modifies and shows the change,
  and each modified region carries a short "Changed for Tab Atelier Remote"
  comment in the code itself so the notice survives being read out of context;
- not use upstream's marks (§6). The app is named and described as Tab Atelier
  Remote, and every user-visible link points at this project, not ConnectBot's.

`../NOTICE` carries the full attribution, including the upstream base commit.

## Why the change set is small

The obvious way to fork is to rename the package to `fr.wdes.tab_atelier`
everywhere — about 160 of the ~380 source files — and then edit them. That
would make every upstream sync a rebase across a rewritten tree.

Android does not require it. `applicationId` is independent of the Kotlin
package, so the app is `fr.wdes.tab_atelier` (which the ADI ownership token and
the installed app's upgrade path both require) while the code stays
`org.connectbot` on disk. The change set then only has to touch the build config,
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

### Adding a string

Strings this fork adds go in
`connectbot/app/src/main/res/values/strings_tabatelier.xml` — a file upstream
does not have, so adding a string is adding a line to it, and an upstream sync
has nothing to rebase. Android's resource merger combines every file under
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

**The contact screen behind those buttons is gone, not just unlinked.** Disabling
the button was the first half of that decision — the app must not push a user at
upstream's community, for licence and product reasons both — and the screen it
reached, plus its route, its destination constant and its tests, went with the
button. Leaving the route in place was a trap rather than a saving: a route with
no entry point is dead weight, and restoring one button later would have silently
resurrected ConnectBot's IRC and mailing-list screen inside this product, which is
exactly what the decision said not to do.

This is the one place the fork subtracts rather than adds, so it is worth being
explicit about the cost: `ContactScreen.kt` no longer exists here, and
`NavDestinations.kt`, `NavGraph.kt` and `HelpScreen.kt` carry the removal. Every
other change in this document adds or gates; this one deletes, and an upstream
sync will offer the screen back. Take it and re-apply the removal, or leave it
and the route returns with nothing linking to it.

### 0004 — the tab-atelier server type

> The numbers in these headings are **historical**: each was one patch file in the
> old `overlay/patches/`, and they are kept as labels because the sections below
> refer to each other by them (`see 0004's section above`). There are no patch
> files now — the changes are commits that touch `connectbot/`, and the headings
> group the same changes they always did. Where a section says a file "is copied
> in from `files/`", read it as "is a file upstream does not have".

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

New files are added under `connectbot/`, since upstream has nothing like them:
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
output inflated with `GZIPInputStream` — gzip, not a raw deflate stream), and a
"focused" frame as the session opens, which is what makes the daemon stamp the
tab's `last_used_at`. So using a tab is what moves it up the list the list sorts
by.

**The terminal mirrors the daemon's grid; it does not drive it.** A tab is a real
workstation terminal — the one this was built against is 193 columns — and the
daemon *refuses* to resize it, on purpose: a phone viewer must not reflow a shared
PTY out from under an agent's TUI or another viewer. So a client that renders at
its own width gets the tab's whole scrollback replayed with cursor positions
computed for a different geometry, and every line wraps somewhere new. The
symptom is a session that looks like several terminals at once, with only the
last-drawn prompt reading correctly. The fix is the meta frame's `cols`/`rows`
(tag `0x03`), which the terminal adopts as a forced size and fits its *font* to —
the same way the daemon's own browser client behaves on a phone. Three
consequences, each of which looks like a bug if you do not know it:

- **Nothing is sent back about size.** `setDimensions` is deliberately empty. A
  resize frame is documented as a server-side no-op, and this protocol's
  reference client never sends one — a mirror that tells the server its size is
  asking a future daemon to do the opposite of what it wants.
- **The preview paint (tag `0x0c`) is not rendered.** It carries the last screen
  as text, sent ahead of a large catch-up so a viewer paints immediately, and it
  is a hint rather than a stream: feeding it to the emulator concatenates it with
  the replay that follows, drawing the same screen twice.
- **A profile's forced size still wins**, and every other transport is unaffected,
  because the flow a transport would report through starts null.

**The tab's name comes from the same meta frame**, so the console titles the
session "server - tab". A session is "which server" and "which of its tabs", and
the host's name alone cannot say the second — two tabs of one server would title
themselves identically. It is taken from the daemon rather than from the row that
was tapped so that it stays right if the tab is renamed while the session is open,
and so a session restored from saved state shows the current name rather than the
one that was on screen when it was saved. Every other transport reports no name
and keeps the host's nickname alone.

One trap in reading it, which would have shipped: **`org.json`'s `optString`
returns the *string* `"null"` for a JSON null**, not the fallback — so the
daemon's own "unknown" sentinel would have titled the session
`server - null`. The frame is read with `has`/`isNull` first, and only a real
non-blank string is a name.

Which tab to open cannot ride on the host row — a row is the daemon, not one of
its tabs, and a WebSocket cannot be asked for a tab once it is open. So the id
travels from the tap through the console route's optional `?tab=` argument and is
*consumed* by the transport (taken, not read), so a later session cannot inherit
the previous one's tab.

**Another tab of a server that already has a session moves that session.** It does
not open a second one, and it used to be refused: `TerminalManager` keys a session
by host, so `openConnection` threw "Connection already open for that nickname" and
tapping a second tab left the user looking at the first. One session per server is
worth keeping — it is what the running notification, the host list's connected
indicator and the session maps all assume — and moving loses nothing, because the
app is a viewer and the tab being left keeps running on the daemon.

The difficulty is that a move must not look like an ending. `Relay` reads
`read()`, and `-1` ends its read loop, so a switch done by closing this transport
and building a new one would kill the session it is moving. So:

- The switch happens **inside** the transport (`switchTab`), which keeps its
  identity; between the two sockets `read()` reports `0`, "nothing right now",
  which is what a socket with no bytes yet reports anyway.
- The socket being replaced is **detached before it is closed** (`currentAttempt`),
  because its callbacks still run — `onClosed`, and possibly `onFailure` for one
  still handshaking — and either would otherwise be read as the session ending.
- The **inbound queue is dropped** but nothing else is: frames of the tab being
  left are not the tab being opened, and the replay that follows would be drawn
  underneath them.
- The screen is **not** cleared explicitly. The daemon's replay opens with a form
  feed, so the new tab's first bytes clear before they repaint — which is what the
  daemon's own browser client relies on for exactly this. That client calls
  `term.reset()` only to wipe a preview frame, which this app no longer renders.
- A move that fails leaves the session reporting the failure, the same as any
  other lost connection, rather than sitting there looking alive.

Tapping the tab already open is a no-op, not an error, which is why the manager
returns the existing session whatever the move did — falling through to
`openConnection` in that case would hit the "already open" refusal.

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

### 0005 — console-screen behaviour

One file, `ui/screens/console/ConsoleScreen.kt`, deliberately its own patch rather
than folded into 0004: it is not about the tab-atelier server type, it applies to
every transport, and upstream may well want it. Two changes, both about what the
console does around a session rather than how it talks to one.

**Leaving a terminal puts the keyboard away.** The app is a single Activity, so
the IME outlives the console screen: backing out to the host list left the
keyboard up over a list with no text field in it. The fix hides it imperatively
from a `DisposableEffect` on dispose. Clearing `showSoftwareKeyboard` would not do
— disposal is the last thing the composition does, so there is no recomposition
left to act on a state change. It is guarded on `isChangingConfigurations`, so
rotating the device mid-session does not close the keyboard: MainActivity does not
handle orientation itself, so a rotation disposes this screen without the user
having left anything.

**The terminal adopts the remote's grid size** when one is reported, which is what
makes a tab-atelier tab render as one screen rather than several — see the grid
mirror in 0004's section above. A profile's forced size still wins, since that is
a deliberate choice by the user.

### 0006 — the bridge reports the remote's grid

One file, `service/TerminalBridge.kt`: a `StateFlow` holding the rows×cols a
transport reports, plus the setter a transport calls. It lives on the bridge
because that is what the console is already observing, so there is no second
channel to keep in sync — and being a `StateFlow` that starts `null`, it costs
every other transport nothing at all, since they never set it and the console
therefore never forces a size for them.

Non-positive values are ignored rather than stored: a 0-column terminal is not a
size, and a daemon that sent one would have the client render nothing at all.

**This file is the one the old overlay could not have kept, and that is the whole
argument for the subtree.** Under the overlay, a modified file existed only if a
patch said so, so an edit that nobody wrote into a patch was silently reverted by
the next `apply-overlay.sh` — and the reproduction check then compared two trees
that *both* lacked it, so it passed. `service/TerminalBridge.kt` was edited that
way, and the trap was live until it was given the patch that is now this section's
commit.

Nothing can be lost that way now, because a change is a commit or it is nothing:
there is no second copy of the truth to fall out of step with. The transferable
part of the lesson is the checking, which the overlay taught the hard way and
still applies when a subtree sync is verified: **a hash comparison proves nothing
on its own**, because two trees that both lack a change compare equal. Compare
content — a `diff` of `git ls-tree` output, or a `grep` for a symbol the change
introduces — or the check will happily confirm a change that is not there.

### 0007 — a two-row, borderless keys bar

`ui/components/TerminalKeyboard.kt` and the console's `TerminalKeyboard(` call.
Upstream's bar is one horizontally-scrolling row of bordered keys, so the key a user
wants is at an unpredictable position off-screen and every key is drawn with an
outline. This replaces it with a two-row layout:

    ESC  /  -  HOME↑  END  PGPREV            FN
    TAB  CTRL ALT  ←  ↓  →  PGNEXT          ⌨

`FN` swaps the keys for an F1–F12 page whose trailing button is a back arrow, and
paste takes the other trailing slot. `PGPREV`/`PGNEXT` are the only keys whose
meaning differs from the request that produced this: **they send PageUp/PageDown
rather than scrolling the local scrollback**, because termlib 0.3.10 exposes no
scroll controller to app code — `onScrollControllerAvailable` belongs to
`TerminalWithAccessibility`, not the `Terminal` this app calls, and
`ScrollController` is internal to the library. JuiceSSH's behaviour needs a termlib
change, not one here. The compose-mode key is gone because the console's own menu
already toggles it and the layout has no slot; a test pins its absence so a later
upstream sync cannot resurrect it silently.

Two of upstream's keys were *added* rather than removed, and one of their preferences
is now inert:

- **Shift** did not exist in upstream's bar, so `Shift`+`Tab` could only be typed on a
  hardware keyboard. The key latches through the same `metaPress` cycle `Ctrl` and
  `Alt` use — `OFF` → `TRANSIENT` → `LOCKED` — and `LOCKED` is what makes it usable
  one-handed: tap it twice to hold it, then tap `Tab`. That state is upstream's
  `ModifierLevel.LOCKED`, which a French keyboard paints as `Verr Maj`. Nothing had to
  change underneath: `TerminalKeyListener` already converted `OUR_SHIFT_MASK` for the
  encoder, so only the key was missing.
- **The soft-keyboard key is always present**, below `FN`. It was behind upstream's
  `showImeToggleKey` preference, which is what left that slot empty and made the bar
  look as though a key were missing from it. The parameter is gone from the bar rather
  than defaulted, so nothing can gate it; the preference is still on the settings
  screen and no longer affects this bar, which is a deliberate choice — removing the
  row from their preferences screen would be a larger fork delta than the problem, and
  a key that is always there is what this bar was for.

**Two of the four bugs this took would have shipped a bar whose keys could not be
pressed, and both were invisible in the source:**

- `KeyButton` sized every key with a hardcoded width, and a fixed width *beats* the
  `Modifier.weight(1f)` a caller passes. A row of seven keys therefore stayed seven
  fixed widths wide however narrow the phone was, so the columns laid out after them
  — `FN`, the keyboard toggle, paste — fell outside the window. Present, enabled,
  labelled, unpressable, and device-dependent: it worked on a wide screen.
- Two keys in the row had no weight, and a key's content fills whatever it is given,
  so each took the **whole** row and left its neighbours at zero width — `Esc`
  measured 0×0. A key missing its weight breaks its neighbours rather than itself,
  which is what made it hard to attribute.

Both were found by measuring bounds rather than reading code, and the measurement is
the reason to keep the shape: a key that exists, is enabled and is labelled can still
be impossible to press, so a test that asserts presence proves nothing. The bar's
tests press each key and check the callback fires.

`KeyButton` also read `contentDescription` only in its icon branch, so passing one
with a text key dropped it in silence — `FN` had no accessible name. Fixing it is
what makes "Show function keys" audible rather than the two letters.

Four smaller faults in the bar were fixed after the layout itself, all reported from
use rather than found by review:

- **`Esc` and `Tab` ignored a latched modifier.** Both called
  `keyDispatcher.dispatchKey(0, …)` and then `clearTransients()`, where every other key
  passes the state built from Ctrl, Alt and Shift. A latched modifier was therefore
  eaten without reaching the terminal, which is why Shift+Tab was impossible
  one-handed; both now pass what the rest of the bar passes.
- **`Tab` was labelled `⇥`**, a symbol no keyboard prints. It is the word now — "TAB" in
  English, "Tab" in French, so a translation rather than a notation. Upstream has no
  such label, so it is a string of ours.
- **The two rows had different key counts** — seven above, eight below — so a weight
  divided each row differently, every column was off by one, and the up arrow sat above
  the left arrow rather than above the down arrow. The `|` key the layout was specified
  with had been dropped; restoring it makes both rows eight. **A key present in one row
  and not the other shifts that row's whole grid**, so the two have to be counted
  together — reading either row on its own, both look correctly laid out.
- **The function keys** use upstream's `button_key_f1`…`f12` rather than a format string
  of ours. An earlier attempt reused `automation_key_function`, whose French turned out
  to be a machine translation, "F %1$s", with a space where `F1` belongs.

### The terminal's own answers are not typed into the session

Not one of the numbered patches — this postdates the patch set, and the code is a
subtree now — but it belongs in this record, because it reads like something a cleanup
would remove as unnecessary.

`TerminalBridge` wires the emulator's reply channel straight to the session:

    onKeyboardInput = { data -> transportOperations.trySend(WriteData(data)) }

That exists so a terminal can answer a program that asks it something. What makes it
wrong here is the replay: a tab-atelier session sends the tab's whole scrollback when a
viewer attaches, and that scrollback contains the query sequences other programs
printed — so the emulator answers questions nobody asked, on every attach, and the
answers arrive in the shell as input. A bash prompt grew
`|libvterm(0.3)` followed by `ESC[?1;2c`: this terminal's XTVERSION reply and its DA1
reply, typed into a prompt. The daemon's own browser client documents the same hazard
and disables those replies for the same reason (`assets/main.js`).

Two reply shapes are now dropped, both self-describing and useless on this path: the
device-attribute replies (`ESC [ ? … c`, `ESC [ > … c`) and device-control strings
(`ESC P … ESC \`), which is how XTVERSION answers. **A cursor-position report is
deliberately kept** — a program asks where the cursor is while it is waiting for the
answer, so dropping that would break the programs that use it rather than the replay
that does not. The asymmetry is the point, and `isTerminalQueryReply` says so.

Finding it took a wrong turn worth recording. The app was first ruled out on two true
facts — the emulator's Kotlin API has no reply channel, and the daemon's own emulator
answers with different bytes — and neither was decisive, because the reply path is in
the **native** library: `libjni_cb_term.so` is libvterm, and holds `>|libvterm(%d.%d)`
and `?1;2c` verbatim. Stopping at the API surface produced a confident answer that was
wrong.

### Two numbers that had to agree, and a border that is not ours

Three later fixes, each of which reads like something to remove or a detail to skip if
the reason is not written down.

**The terminal's green outline is termlib's, and is clipped away.** `TerminalBridge` and
the console draw no border; that one is inside the library, unconditionally, with the
colour hardcoded — `Color(0xFF4CAF50)`, Material Green 500, at 0.6 alpha — and no
parameter to turn it off. The console **clips** the terminal's drawing to remove it rather
than painting over it, and the distinction is the whole design: a mask would have to be
the terminal's own background, which is the tab's colour for a tab-atelier session and
whatever scheme the profile uses for every other transport — so a mask would need to know
both, and would draw a frame of the wrong colour whenever it guessed. A clip needs to know
neither: nothing is drawn where the border was, so the console's backdrop shows through,
which is already correct for every tab and every transport.

The clip width is a judgement, because the outline's width is a private constant beside
its colour and cannot be asked for. `TERMINAL_EDGE_CLIP_DP` is one constant for that
reason: if a sliver of green survives on a device, that is the number to raise.

**The key bar is an overlay, so the terminal has to be padded clear of it.** The bar is
drawn with `align(Alignment.BottomCenter)` over the terminal, so nothing sizes the
terminal area down for it — a `bottom` padding does, and it reserved one key row's height
while the bar had become two. The bar therefore sat over the terminal's last row and hid
it, with nothing failing and nothing looking wrong. Both numbers now derive from
`TERMINAL_KEYBOARD_BAR_HEIGHT_DP`. Note `keyboardAlwaysVisible` defaults to **true**, so
that padding applies by default.

**Pull-to-refresh re-asks every tab-atelier server.** Per server rather than per row,
because the gesture is made on the list and means "all of this may be stale", and it
forces the probe — which is what makes it useful, since the automatic probe is skipped
when a server's own settings have not changed. The indicator follows the fetches rather
than a flag of its own, so a failed refresh cannot leave it spinning; `fetchTabs` keeps
the tabs it already has, so the list does not blank while it turns.

The first two are the same fault as the two key rows needing equal counts: **values that
have to agree**, kept in a shape where nothing makes them. Where that recurs, the fix is
to derive one from the other rather than to remember both.

### Pinning a tab, and filtering a server's tabs by name

Two additions to the tab list, both stored beside the host pins in
`tabatelier_tab_state` — desktop-side state that belongs to the user rather than to the
daemon:

- **A tab can be pinned**, and pinned tabs sort above the rest of that server's. The order
  underneath is the daemon's, which is most-recently-used first, so it carries information
  — the sort is stable, and a version that reordered the unpinned tabs as a side effect
  would be losing something. Pins are keyed by the tab's **id**, not its position, so a pin
  follows its tab when the order changes.
- **A server's tabs can be filtered by name**, from a field shown above them whenever that
  server has tabs to filter. A query persists, so it survives a rotation or a restart, and
  it is a query rather than a mode: the tab list keeps refreshing underneath it.

Both live on `TabListState`, and `fetchTabs` **carries them across from the state it
replaces** — it builds a fresh state per fetch, so without that a 15-second refresh would
clear the filter and the pins. That is worth knowing before adding anything else to that
class: the field is not preserved by construction, only by the two `copy` calls that carry
it.

The filter and the pins are read on the **loading** path, so both go through
`readTabUiPrefs`/`writeTabUiPrefs`, which swallow a failure and return a default. A pin and
a filter are conveniences; losing them must not be able to stop tabs loading, and the first
version — which called `getSharedPreferences` directly — did exactly that, because a
preferences store that cannot be opened threw inside the fetch coroutine and no tab
appeared at all.

A filter that matches nothing says **so**, with the query in the message, rather than
showing the "no tabs" note: "this server has no tabs" and "none match what you typed" call
for different reactions, and the first when the second is true reads as the server having
lost its tabs.

## Current state

The app is a working ConnectBot under our package id, plus the tab-atelier type:
servers can be added, their tabs are listed and ordered by last use, and tapping
one opens that tab's terminal. Nothing upstream is removed — SSH, telnet, mosh
and the local shell all still work, and nothing upstream is deleted. A future
milestone
may retire the transports that have no use here, but only if that is wanted: the
additive shape is what keeps an upstream sync to a pin bump.

The session path has now been run against a real daemon, which is how the bugs
below were found — none of them by the unit tests, all of them against a live
server:

- **The terminal rendered a 193-column workstation terminal at phone width**, so
  a tab looked like several terminals at once with one readable prompt line at the
  bottom. The daemon replays a tab's whole scrollback with cursor positions
  computed for its own geometry and refuses to be resized, so the client has to
  mirror that geometry — see the grid mirror in 0004's section above. The
  readable line was the `0x0c` preview paint, which was being fed to the emulator
  as output and so drawn on top of the replay.
- **The transport never told the bridge it was connected.** `bridge.onConnected()`
  is what creates the Relay, which is what reads the transport; every other
  transport calls it, `TabAtelier` did not, so the terminal sat on "connecting
  via tabatelier…" forever. `connect()` now waits for the WebSocket to open and
  then signals it, which is also why it may block — it runs on the io dispatcher.
- **The chevron that hides a daemon's tab list could not be clicked**, because a
  row's trailing content is a stack: it and the overflow button occupied the same
  coordinates. See the layout note in 0004's section above.
- **The daemon refuses the token as an `Authorization` header on a WebSocket
  upgrade** and accepts it only as `?token=`. This is a daemon-side bug:
  `extract_token` (`src/api_ws.rs`) read the query first and the header second,
  but the query lookup was `req.uri().query()?`, whose `?` returns from the whole
  function when there is no query — so a bare `/tabs/by-id/{id}/ws` never reached
  the documented header fallback and 401s. Verified by raw handshake: header
  alone → 401, `?token=` → 101. Fixed on main; the app still uses the query form,
  which works against a daemon that has not been redeployed.

Not yet exercised on a real device: everything above is measured from the host
against a live daemon over the app's own code paths, not from the phone. In
particular the grid mirror's *font* fitting — whether a 193-column grid is
readable at arm's length — is a judgement only a device can make.

