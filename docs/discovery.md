# LAN discovery — proposal

Status: **proposal only**, not implemented. Edit/reject freely.

## Problem

`Preferences::remote_endpoints` is a static list of `https://host:port` +
bearer token + TLS cert fingerprint. Adding every machine at every
location (home, office, dev boxes, the box at $place) means a long
manually-maintained list. The desktop app should find `tab-atelier-headless`
instances on whatever LAN it is currently on, so the user only ever
maintains tokens — not URLs.

## Protocol: mDNS / DNS-SD

- **Service type:** `_tab-atelier._tcp.local.`
- **Port advertised:** 7891 (the TLS listener). The plain `:7890` listener
  is meaningful for loopback / trusted-LAN setups but is not the right
  thing to advertise.
- **TXT records:**
  - `v=1` — protocol version. Future revisions break compatibly.
  - `fp=<hex sha256>` — TLS cert fingerprint, lets the GUI pre-fill
    `RemoteEndpoint::cert_sha256` and reject MITM before the user is
    even asked.
  - `h=<hostname>` — short human-friendly fallback when reverse-DNS is
    ugly.
  - `feat=happier-bridge,catbus` — capabilities, lets the picker show
    "✓ relay available" or similar.

### Why mDNS over a custom UDP broadcast

- Already firewall-traversable on most LANs: macOS ships Bonjour, Linux
  desktops ship avahi, Windows 10+ has built-in mDNS responder.
- Cross-subnet within a /24 without configuration.
- One Rust crate (`mdns-sd`, pure-Rust, no native deps, works on Linux
  + Windows + macOS).
- A custom UDP broadcast would re-invent port allocation, retry/jitter,
  IPv6 multicast group choice, NAT-helper logic. mDNS already solves it.

## Use system DNS-SD APIs — no bundled mDNS responder, no static file

Every modern OS already runs a DNS-SD service. We register with it
rather than ship our own:

- **Linux:** `avahi-daemon` (D-Bus interface `org.freedesktop.Avahi`)
- **macOS:** `mDNSResponder` (`libdns_sd`, system framework)
- **Windows 10 1809+:** `DnsServiceRegister` / `DnsServiceBrowse` in
  `windns.dll`

Wins:

- No second mDNS responder fighting over `:5353` with avahi/Bonjour.
- No extra background thread, no UDP socket, no bound port in the
  headless process. D-Bus client traffic is `AF_UNIX` — already
  permitted by `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`.
- No filesystem write under `/etc` (would otherwise need to widen
  `ProtectSystem=strict` — see "Why not the avahi service file"
  below).
- Cross-platform symmetry: announce and browse both use the same
  per-OS native APIs, so there is no `mdns-sd` Cargo dep at all.

### Per platform

**Linux — avahi D-Bus (preferred).** Talk to
`org.freedesktop.Avahi.Server` on the system bus and ask it to
publish via `EntryGroupNew` → `AddService` → `Commit`. The default
D-Bus policy
(`/usr/share/dbus-1/system.d/avahi-dbus.conf`) already allows the
`tab-atelier` user to call EntryGroup methods — only `SetHostName`
is root-only — so no extra policy drop-in is needed.

Use the `zbus` crate (async, pure Rust, no native dep) with proxies
generated from the system-shipped XML at
`/usr/share/dbus-1/interfaces/org.freedesktop.Avahi.{Server,EntryGroup,
ServiceBrowser,ServiceResolver}.xml`. The `EntryGroup` handle has no
explicit `Free` method on the bus — cleanup is owned by the D-Bus
connection lifecycle: when our process exits and the connection drops,
avahi-daemon garbage-collects every EntryGroup we own. `Reset` is
available if we need to clear records without dropping the connection
(e.g. cert rotation). `avahi-daemon` is the only runtime requirement —
`libavahi-client3` is **not** pulled in (we bypass the C client lib).

If `avahi-daemon` is not running on the host, `Server` is simply
unreachable on the bus — log "discovery: avahi-daemon not available,
skipping announce" and move on. Discovery is opportunistic; failure
is non-fatal.

**macOS — `libdns_sd` (preferred).** Always present (system
framework). Either ~40 lines of `unsafe` calling `DNSServiceRegister`
/ `DNSServiceBrowse` from `<dns_sd.h>`, or the `astro-dnssd` crate
(thin wrapper over `libdns_sd`, no external deps on macOS). The
returned `DNSServiceRef` is held for the lifetime of headless;
dropping it deregisters automatically.

**Windows — native DNS-SD (preferred).** Windows 10 1809+ exposes
`DnsServiceRegister` / `DnsServiceBrowse` in `windns.dll`. The
`windows` crate already in our `[target.'cfg(windows)'.dependencies]`
covers it under the `Win32_NetworkManagement_Dns` feature. ~30 lines
of `unsafe`. Headless-on-Windows is not currently shipped, so this is
forward-looking; the desktop GUI Windows install only browses.

### Why not the avahi service file approach (rejected)

Earlier sketch had headless write
`/etc/avahi/services/tab-atelier.service` and let avahi-daemon pick it
up. Two problems killed it:

1. **`ProtectSystem=strict`** in the hardened unit makes `/etc`
   read-only for the process. Allowing it would require
   `ReadWritePaths=/etc/avahi/services` — which simultaneously gives
   us write access to every *other* package's service file in that
   directory. A regression in the hardening profile we just landed.
2. **Stale-file risk on crash.** A crash leaves the file behind,
   advertising a service that's not running. D-Bus EntryGroup
   automatically vanishes when the bus connection drops, so the
   announcement disappears the instant headless dies.

The file approach is fine for *static* services (e.g. CUPS) but not
for a daemon advertising its own runtime TLS fingerprint.

### Code shape

`src/discovery.rs` becomes three platform-gated functions, not a
"daemon":

```rust
#[cfg(target_os = "linux")]
pub fn announce_linux(port: u16, fingerprint: &str) -> AnnounceHandle { /* write file */ }

#[cfg(target_os = "macos")]
pub fn announce_macos(port: u16, fingerprint: &str) -> AnnounceHandle { /* DNSServiceRegister */ }

#[cfg(windows)]
pub fn announce_windows(port: u16, fingerprint: &str) -> AnnounceHandle { /* DnsServiceRegister */ }
```

`AnnounceHandle` is an opaque RAII type that deregisters on drop —
removes the avahi service file, drops the `DNSServiceRef`, calls
`DnsServiceDeRegister`. Called once from `headless::run` after the TLS
listener binds:

```rust
#[cfg(feature = "discovery")]
let _announce = discovery::announce(api_tls_port, &tls_fingerprint);
```

No thread, no event loop, no `ServiceDaemon` struct, no `:5353`
binding on our side.

## Browser side — same system APIs

The browser uses the same per-OS facilities as the announcer:

- **Linux:** `org.freedesktop.Avahi.Server.ServiceBrowserNew("_tab-atelier._tcp", "local")`
  returns a path; subscribe to `ItemNew` / `ItemRemove` / `AllForNow`
  signals on that object. For each `ItemNew`, call
  `Server.ResolveService` to expand into host/port/TXT. zbus.
- **macOS:** `DNSServiceBrowse` + `DNSServiceResolve` via `libdns_sd`
  (or `astro-dnssd`).
- **Windows:** `DnsServiceBrowse` + `DnsServiceResolve` via the
  `windows` crate.

No `mdns-sd` Cargo dep anywhere.

**`src/app.rs`** Preferences dialog `remote_endpoints` section:

- Add a "Discovered on this network" sub-list above the saved list. Each
  entry is one row: `[+] colossus.local — 192.168.1.42:7891`. Clicking
  `+` opens the existing "add endpoint" form pre-filled with URL +
  fingerprint from the TXT record. User only types the bearer token.
- Browser lives on the gpui background runtime, sends `DiscoveredPeer`
  events into AppState via `cx.update_global`.

**`Cargo.toml`** — no cross-platform mDNS crate; per-OS deps gate on
the `discovery` feature:

```toml
[features]
default = ["gui", "energy", "catbus", "discovery"]
discovery = []

# Linux: pure-Rust D-Bus to avahi-daemon. No FFI, no libavahi-client3.
[target.'cfg(target_os = "linux")'.dependencies]
zbus = { version = "5", default-features = false, features = ["tokio"], optional = true }

# macOS: libdns_sd is a system framework — no Cargo dep needed if we
# write the FFI ourselves; astro-dnssd if we want a wrapper.
# [target.'cfg(target_os = "macos")'.dependencies]
# astro-dnssd = { version = "0.3", optional = true }

# Windows: already pulls the `windows` crate; just enable the DNS feature.
[target.'cfg(windows)'.dependencies]
windows = { version = "...", features = [
    "Win32_NetworkManagement_Dns",
    "Win32_Foundation",
], optional = true }
```

Wire `zbus` / `windows` under the `discovery` feature in
`[features]`. Add `discovery` to the headless variant features in
`[package.metadata.deb.variants.headless]` and to the desktop MSI
build line in `.github/workflows/windows-desktop.yml`.

### Debian packaging — the canonical way

The `.deb` headless variant grows two changes:

```toml
[package.metadata.deb.variants.headless]
features = ["headless", "happier-relay-binary", "catbus", "discovery"]
recommends = "avahi-daemon"
```

Why **Recommends**, not Depends:

- Discovery is *opportunistic*. If `avahi-daemon` is absent (servers
  with mDNS deliberately off, embedded boxes), headless still works —
  it just logs "avahi unavailable, skipping announce" and continues.
- Debian Policy §7.2: Recommends is for "packages that would be found
  together with this one in all but unusual installations". That fits
  exactly. A headless server without LAN discovery is unusual but
  legitimate; forcing avahi-daemon onto every install would be
  policy-aggressive.
- Debian/Ubuntu default `APT::Install-Recommends true`, so end-users
  on desktop installs get it automatically. Server admins can
  `--no-install-recommends`.

What we deliberately do **not** declare:

- ~~`Depends: libavahi-client3`~~ — we talk D-Bus directly, the C
  client lib is unused.
- ~~A drop-in in `/etc/dbus-1/system.d/`~~ — avahi's own policy
  already grants our user EntryGroup access.
- ~~A `Conflicts: tab-atelier-discovery` for some hypothetical
  separate-package alternative~~ — discovery is in-binary, feature-gated.

`postinst` echo (cheatsheet block) gains one line:

```
LAN discovery: avahi-daemon advertises this host as
_tab-atelier._tcp on the local network. Verify with:
  avahi-browse -r _tab-atelier._tcp
Disable per-user in preferences.json: "discovery_enabled": false
```

`postrm` does **not** need to touch anything D-Bus-related — when the
headless process exits, its EntryGroup vanishes from the bus
automatically (D-Bus owns the lifecycle, not us).

**`src/lib.rs`** `Preferences`:

```rust
#[serde(default = "default_true", skip_serializing_if = "is_true")]
pub discovery_enabled: bool,
```

Default on. Off-LAN users on hostile networks (coffee shops) can
disable. Preference dialog gets a checkbox under the existing "Remote
endpoints" group.

**`assets/preferences.default.json`** — add `"discovery_enabled": true`.

## Auth model — what discovery does *not* do

Discovery surfaces URL + cert fingerprint. **The bearer token is still
per-pair and is never broadcast.** Today the token lives in
`~/.local/state/tab-atelier/api.token` on the headless box; the user
copies it once.

Two ways to make that one-shot less painful (out of scope for first
cut, listed only so they're remembered):

1. **QR enrollment** — headless prints a QR (label + url + fingerprint
   + token) to `journalctl` on first boot. GUI scans it via webcam or
   the user copies the data-URL.
2. **happier-relay enrollment** — when the relay is reachable, GUI asks
   the relay to mint a scoped token after a challenge-response with the
   headless host.

First cut: discovery fills URL + fingerprint, user pastes token.

## Privacy / security

- `discovery_enabled = false` disables both announcer and browser.
- Announce only on the addr the TLS listener actually bound to. If user
  set `api_tls_addr = "127.0.0.1:7891"`, do not advertise anywhere —
  loopback-only is implicit "do not broadcast".
- TXT-advertised fingerprint is never trusted by itself. The TLS
  handshake on connect still has to present a cert whose hash matches.
  mDNS poisoning gets you a name in the picker, not a connection.
- Document in the postinst echo and `AGENTS.md` that mDNS leaks
  `hostname` + `_tab-atelier._tcp` presence to anyone on the LAN.

## Sequencing

1. Add `discovery` feature; add Linux-gated `zbus` dep.
   Generate proxies from the system-shipped XML at
   `/usr/share/dbus-1/interfaces/org.freedesktop.Avahi.*.xml` (commit
   the generated `.rs` to avoid a build-script avahi-daemon dep).
   Land `src/discovery.rs` with `announce_linux` /
   `browse_linux` skeletons.
2. **Linux announce** — call from `headless::run` after the TLS
   listener binds. Verify externally with `avahi-browse -r _tab-atelier._tcp`.
3. **Linux browse** — wire into Preferences UI as a read-only debug
   list ("found N peers"). End-to-end Linux ↔ Linux test possible at
   this point.
4. `+` button → prefill add-endpoint form. Token still manual.
5. macOS announce/browse via `libdns_sd` (when/if a macOS port is
   wanted).
6. Windows announce/browse via the `windows` crate (announce only
   relevant if tab-atelier-headless ever ships on Windows; browse
   useful immediately for the desktop GUI).
7. (Later) enrollment shortcut (QR or relay-mediated).

Steps 1–4 = one session of work and cover the actual current shipping
configuration (Linux headless + GUI Linux browse). Steps 5–6 unlock
cross-platform discovery from the Windows desktop and a future macOS
build.

## Debugging / verification commands

External tools (no tab-atelier knowledge required):

```sh
# Watch the bus for any Avahi traffic from our process:
sudo dbus-monitor --system "interface=org.freedesktop.Avahi.EntryGroup"

# Browse the network for our service type:
avahi-browse -r _tab-atelier._tcp

# Confirm avahi-daemon is up and reachable on the bus:
busctl --system status org.freedesktop.Avahi
```

## Off-LAN fallback

happier-relay already exists for WAN; discovery is strictly LAN-scoped.
Off-network access still requires either a saved endpoint or the
relay — discovery does not replace either.
