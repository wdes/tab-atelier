<!--
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.
-->

# tab-atelier-proxy

A Claude tab can send its Anthropic API calls somewhere else instead of
straight upstream. `tab-atelier-proxy` is that somewhere: one machine holds a
Claude login, and everyone else authenticates to it with a key of their own.

## Why it is a separate package

It used to be a role of tab-atelier itself — `relay egress` — and it
authenticated the whole fleet with **one shared token**. That single secret:

- cannot say who spent the quota;
- cannot be taken away from one laptop without re-keying every other machine;
- keeps working for a laptop that has left the building.

An account per person with a key each answers all three. And the far end is a
*server*: it belongs on a box that has no display, run by its own system user,
under a systemd unit — not something you should have to install a terminal
emulator and X11 to obtain.

**The near end did not move.** Both tab-atelier editions — desktop and headless
— still speak `relay on`, `relay off` and `relay via`. They just point at this.

## Three credentials, and none of them is the same thing

| | what it opens | where it lives |
|---|---|---|
| **user key** (`tap_…`) | the Anthropic path, and nothing else | a developer's environment |
| **admin token** (`tap_…`) | the account API and the web UI | the proxy host only |
| **Claude OAuth login** | Anthropic itself | the proxy host only, never sent to a client |

A user key travels — it ends up in shell profiles and CI secrets — so it must
not be able to administer anything, and it is not replayable against Anthropic
directly. Presenting the admin token on the proxy path is refused, and says so.

## Install

```sh
sudo apt install tab-atelier-proxy
sudo -u tab-atelier-proxy claude          # log in once; this is the egress login
sudo systemctl enable --now tab-atelier-proxy
```

It listens on `127.0.0.1:7900`. Put a TLS terminator in front rather than
exposing it directly — it carries keys.

## Accounts

```sh
sudo -u tab-atelier-proxy tab-atelier-proxy add Ada Lovelace ada@example.org
sudo -u tab-atelier-proxy tab-atelier-proxy list
sudo -u tab-atelier-proxy tab-atelier-proxy rotate ada@example.org
sudo -u tab-atelier-proxy tab-atelier-proxy disable ada@example.org   # keep the account, stop the key
sudo -u tab-atelier-proxy tab-atelier-proxy remove  ada@example.org   # forget both
```

`disable` and `remove` differ on purpose: disabling keeps the name attached to
past usage, deleting forgets it.

**A key is shown once.** The proxy stores a SHA-256 of it and cannot show it to
you again — lost key, `rotate`. (A fast hash is right here: the key is 32 bytes
of CSPRNG output, so there is no dictionary to run against it, and a slow KDF
would only add latency to every proxied request.)

## Web UI

Browse to the proxy and paste the admin token:

```sh
sudo -u tab-atelier-proxy tab-atelier-proxy admin-token
```

Vue and Bootstrap are served by the proxy itself, with no CDN: a credential
proxy is exactly the kind of thing that runs on a locked-down network, and an
admin UI that needs outbound internet would be useless there — quite apart from
giving a third party a script tag on the page where the admin token is typed.

## Pointing a tab-atelier at it

On the machine with the terminal:

```sh
tab-atelier remote add proxy --url https://proxy.example.org --relay-token tap_…
tab-atelier relay via proxy
tab-atelier relay on
```

Claude tabs opened after that route through the proxy.

## Migrating from `relay egress`

The old role is refused now, with a message pointing here — deliberately, since
a relay that keeps half-working after its credential model changed is worse
than one that tells you what to install. On the box that was the egress:

1. `apt install tab-atelier-proxy`
2. add an account per person, hand out the keys
3. on each client, replace the shared token: `remote add … --relay-token <their key>`

The old shared token stops being anything at all — there is nowhere left that
accepts it.

## Where state lives

`/var/lib/tab-atelier-proxy` (0700), holding `users.json` and `admin.token`,
both 0600. Purging the package does **not** delete it: removing a package
should not silently revoke everyone's access with no way back.
