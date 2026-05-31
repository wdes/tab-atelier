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

## File layout

**New** `src/discovery.rs` — feature-gated `discovery`, default-on for
both `gui` and `headless`:

- `pub struct Announcer` — wraps `mdns_sd::ServiceDaemon::register()`.
  Called from `headless::run` after the TLS listener binds. Pulls
  fingerprint from `api::tls_cert_fingerprint()` (already exposed for
  the Preferences "Pin" flow — reuse).
- `pub struct Browser` — wraps
  `ServiceDaemon::browse("_tab-atelier._tcp.local.")`, emits
  `DiscoveredPeer { id, label, url, fingerprint, last_seen }` on an
  `mpsc::channel`.
- `pub fn shutdown(&self)` — both sides honour
  `crate::SHUTDOWN_REQUESTED`.

**`src/headless.rs`** ~line 421, just after `start_api_server_tls`:

```rust
#[cfg(feature = "discovery")]
let _announcer = discovery::Announcer::start(
    &api_tls_addr,
    &tls_fingerprint,
    hostname,
);
```

Resolve `0.0.0.0` to the actual non-loopback v4/v6 addrs at announce
time — `mdns-sd` wants real IPs, not wildcards.

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

[dependencies]
mdns-sd = { version = "0.13", optional = true }
```

Add `discovery` to the headless variant features in
`[package.metadata.deb.variants.headless]` and to the desktop MSI build
line in `.github/workflows/windows-desktop.yml`.

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

1. Add `discovery` feature + `mdns-sd` dep. Land empty
   `src/discovery.rs` with stub `Announcer::start` / `Browser::new`.
2. Wire announcer in `headless::run`. Verify with
   `avahi-browse -r _tab-atelier._tcp` on a Linux box.
3. Wire browser into Preferences UI as a read-only debug list ("found
   N peers").
4. Add the `+` button → prefill add-endpoint form. Token still manual.
5. Cross-test Linux ↔ Windows headless discovery once the desktop MSI
   is verified booting.
6. (Later) enrollment shortcut (QR or relay-mediated).

Steps 1–4 are one session of work. 5 needs the Windows install
verified separately. 6 is its own design.

## Off-LAN fallback

happier-relay already exists for WAN; discovery is strictly LAN-scoped.
Off-network access still requires either a saved endpoint or the
relay — discovery does not replace either.
