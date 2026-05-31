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

## Announcer: delegate to the system, do not run our own

Original sketch had `src/discovery.rs` spin up an `mdns_sd::ServiceDaemon`
inside the headless process — that's a second mDNS responder racing the
one the OS already runs. Every Linux desktop ships `avahi-daemon`; macOS
ships `mDNSResponder`; Windows 10 1809+ ships native DNS-SD APIs. Use
those — register a service with the OS responder instead of bringing our
own.

Wins:

- No extra background thread or socket inside headless. The OS daemon
  already holds the multicast group and answers queries from any process.
- Two responders on the same host fighting over `:5353` is a known
  source of "works for some clients, not for others" bugs. One responder
  per host is the contract avahi/Bonjour assumes.
- On Linux the cleanest path is fully declarative (drop an XML file in
  `/etc/avahi/services/`) — zero Rust code for the announce side, the
  package install can do it.

### Per platform

**Linux — avahi service file (preferred).** avahi-daemon watches
`/etc/avahi/services/*.service` and (re-)publishes anything dropped
there. Plan: headless writes `/etc/avahi/services/tab-atelier.service`
on startup with the live TLS fingerprint baked into a `<txt-record>`,
and removes it on clean shutdown. The file is small (<1 KB) and the
write is a single fsync. No D-Bus, no FFI, no extra dependency.

Skeleton (template; fingerprint + port substituted at runtime):

```xml
<?xml version="1.0" standalone='no'?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group>
  <name replace-wildcards="yes">tab-atelier on %h</name>
  <service>
    <type>_tab-atelier._tcp</type>
    <port>7891</port>
    <txt-record>v=1</txt-record>
    <txt-record>fp=&lt;hex sha256&gt;</txt-record>
    <txt-record>feat=happier-bridge,catbus</txt-record>
  </service>
</service-group>
```

If avahi-daemon is not running (servers without it), fall back to
talking the system D-Bus interface (`org.freedesktop.Avahi.Server`)
via `zbus` — still no extra mDNS stack in our process.

**macOS — `libdns_sd` (preferred).** Always present (system framework).
~40 lines of `unsafe` calling `DNSServiceRegister` from `<dns_sd.h>`,
or use the `astro-dnssd` crate (thin wrapper, no external deps on Mac).
The returned `DNSServiceRef` is held for the lifetime of headless;
dropping it deregisters automatically.

**Windows — native DNS-SD (preferred).** Windows 10 1809+ exposes
`DnsServiceRegister` in `windns.dll`. The `windows` crate already
bundles its bindings. ~30 lines of `unsafe`. For Win10 < 1809 (rare
on the boxes we target) and headless-on-Windows-without-Bonjour,
fall back to `mdns-sd` — but headless-on-Windows is not currently a
shipped configuration, so this fallback is unlikely to fire in
practice. The desktop GUI Windows install does not announce (it is a
client only).

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

## Browser side

Browsing is a different question — we have to actively listen for
service announcements. Options:

- **`mdns-sd`** (pure-Rust): in-process socket on `:5353` in passive
  query mode. Conflicts with system responder only when we try to
  *publish* — passive querying coexists fine.
- **avahi (Linux)** via D-Bus `org.freedesktop.Avahi.ServiceBrowser`
  / **`libdns_sd` browse on macOS / Win10+**: mirror the announcer
  choice for symmetry.

Lean toward `mdns-sd` for the browser to keep the GUI cross-platform
without a per-OS code path. The desktop install already pulls
`mdns-sd` for the browser even if announce is system-delegated, so we
pay the dependency once. (If pure-Rust dep budget matters more than
cross-platform symmetry, swap browser to system-delegated too.)

**`src/app.rs`** Preferences dialog `remote_endpoints` section:

- Add a "Discovered on this network" sub-list above the saved list. Each
  entry is one row: `[+] colossus.local — 192.168.1.42:7891`. Clicking
  `+` opens the existing "add endpoint" form pre-filled with URL +
  fingerprint from the TXT record. User only types the bearer token.
- Browser lives on the gpui background runtime, sends `DiscoveredPeer`
  events into AppState via `cx.update_global`.

**`Cargo.toml`:**

```toml
[features]
default = ["gui", "energy", "catbus", "discovery"]
discovery = ["dep:mdns-sd"]

# mdns-sd is used by the browser only. The announce side uses
# system facilities: avahi service file on Linux (no dep),
# libdns_sd on macOS (system framework, no Cargo dep), and the
# `windows` crate's already-vendored DNS-SD bindings on Windows.
[dependencies]
mdns-sd = { version = "0.13", optional = true }
```

Add `discovery` to the headless variant features in
`[package.metadata.deb.variants.headless]` and to the desktop MSI build
line in `.github/workflows/windows-desktop.yml`.

The headless `.deb` postinst should also ensure `avahi-daemon` is at
least *recommended* — `Recommends: avahi-daemon` in
`[package.metadata.deb.variants.headless]` so Debian/Ubuntu pulls it in
by default. If the user explicitly opts out, the service file is
harmless (just an unread file in `/etc/avahi/services/`).

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

1. Add `discovery` feature + `mdns-sd` dep (browser side only).
   Land `src/discovery.rs` with platform-gated `announce()` /
   `browse()` skeletons.
2. **Linux announce (file-based)** — write
   `/etc/avahi/services/tab-atelier.service` from `headless::run`,
   delete on clean shutdown. Verify externally with
   `avahi-browse -r _tab-atelier._tcp`.
3. Wire browser into Preferences UI as a read-only debug list
   ("found N peers"). End-to-end Linux ↔ Linux test possible at this
   point.
4. Add the `+` button → prefill add-endpoint form. Token still manual.
5. macOS announce via `libdns_sd` (when/if a macOS port is wanted).
6. Windows announce via `DnsServiceRegister` (only if/when
   tab-atelier-headless ships on Windows; not currently shipped).
7. (Later) enrollment shortcut (QR or relay-mediated).

Steps 1–4 are one session of work and cover the actual current
shipping configuration (Linux headless + cross-platform GUI browser).
5 and 6 wait until those platforms grow a headless variant.

## Off-LAN fallback

happier-relay already exists for WAN; discovery is strictly LAN-scoped.
Off-network access still requires either a saved endpoint or the
relay — discovery does not replace either.
