# Plan — the viewer as the tab's ssh-agent

**Status: proposal.** Nothing here is built. It extends
[per-tab ssh-agent](./ssh-agent.md), which today spawns a real `ssh-agent`
*on the host* and optionally loads a key file into it.

## The idea

Turn the relationship around. Instead of the host holding a key on behalf of
whoever is watching, the **watcher holds the key** and the host holds nothing:

```
  tab's shell / git / ssh
        │  AF_UNIX  ($SSH_AUTH_SOCK)
        ▼
  daemon: router only — no keys, no crypto, no parsing beyond one byte
        │  one claimed holder per tab
        ├────────────── xterm.js viewer      → WebAuthn / passkey (sk-* keys)
        ├────────────── mobile app           → Android Keystore (StrongBox)
        └────────────── `remote attach`      → the operator's own ssh-agent
```

The private key never reaches the machine running the tab. For a fleet of
agent tabs on a shared box — or a remote one you don't fully trust — that is
the difference between "my deploy key is on that host" and "my phone can be
asked to sign, and I can see what for".

## Why it fits

The daemon barely has to do anything, because both clients already have a
duplex channel:

- **xterm.js / mobile WebView**: `api_ws.rs` is a tagged binary protocol with
  frames in both directions (`0x01 in` C→S, `0x02 out` S→C, …). Two more tags
  carry the agent traffic.
- **Sidecar** (`remote attach`): `RemoteCommand` / `RemoteEvent` already pair a
  client→remote command channel with a remote→client event stream.

The ssh-agent protocol is itself request/response over a stream, with each
message framed as `u32 length || payload`. So the daemon reads exactly one
message, hands the payload over opaquely, and writes back whatever comes
back — it never needs to understand a key or a signature.

## Wire additions

WebSocket (`api_ws.rs`):

| Tag  | Name        | Dir | Payload                                  |
|------|-------------|-----|------------------------------------------|
| 0x0B | agent-req   | S→C | `u32 req_id` + one agent message         |
| 0x0C | agent-resp  | C→S | `u32 req_id` + one agent message         |
| 0x0D | agent-claim | C→S | JSON `{"claim":true}` / `{"claim":false}` |

Sidecar (`remote.rs`): `RemoteEvent::AgentRequest { remote_id, req_id, blob }`
and `RemoteCommand::AgentResponse { remote_id, req_id, blob }`. Same shape,
delivered on the existing poll.

`req_id` exists because a tab can have several `ssh` processes at once; without
it the second one's answer would land in the first one's socket.

## What the daemon enforces

It inspects exactly one byte — the message type — and answers everything
outside the auth path itself with `SSH_AGENT_FAILURE (5)`:

| Type | Name                        | Forwarded? |
|------|-----------------------------|------------|
| 11   | `REQUEST_IDENTITIES`        | yes        |
| 13   | `SIGN_REQUEST`              | yes        |
| 17/18/19/20/21/22/25/27 | add / remove / lock / extension | **no** — `FAILURE` |

So a tab can ask "which keys do you have?" and "sign this", and nothing else.
It cannot add a key to your phone, remove one, or lock your agent.

Two more daemon-side rules:

- **No holder, or no answer within ~10 s → `FAILURE`.** `ssh` then falls
  through to its next auth method instead of hanging on a phone in a pocket.
- **RO share tokens may never claim the holder role.** A read-only viewer is a
  spectator; signing is a write capability. The claim frame requires the RW
  token, same gate as `input`.

## The part that actually matters: consent

Forwarding an agent into a tab is the thing every SSH guide tells you not to
do, and a tab running an autonomous agent is exactly the case they have in
mind. Anything in that tab can ask the holder to sign anything, at any moment,
for as long as the holder is connected.

This design does not pretend to fix that. It makes it *visible and bounded*:

- **Per-signature confirmation is the default**, the equivalent of
  `ssh-add -c`. Allow-once / allow-for-5-min / deny, on the holder's screen.
- The holder can decode the sign request before showing it — it carries the
  key blob and, for user auth, the session id, the username and the service —
  so the prompt reads "sign for `git@github.com` as `williamdes`?" rather than
  "sign 194 bytes?". Decoding happens **client-side**; the daemon stays dumb
  on purpose, so it holds no material and no interpretation.
- Grants are scoped to one tab and expire. A new tab claims nothing.
- Every request is logged with the tab, the key fingerprint and the verdict.
- Off by default, per tab: `ssh-agent <tab> --proxy`.

The boundary is the human tapping "allow" on a screen that says what is being
signed. Everything else is plumbing.

## Three holders, in order of effort

**1. Sidecar — build this first.** `remote attach` runs on your laptop, which
already has a real `ssh-agent`. The client relays the blob to its own
`$SSH_AUTH_SOCK` and relays the answer back. No crypto in our code, no new UI,
and it is textbook agent forwarding: the remote tab can `git push` using the
key on the machine you're sitting at. This slice validates the whole router
with the least new surface.

**2. Mobile app.** An EC P-256 key in the Android Keystore (StrongBox where
available) is an `ecdsa-sha2-nistp256` SSH key: `REQUEST_IDENTITIES` returns
its public blob, `SIGN_REQUEST` is a Keystore sign guarded by a biometric
prompt. The confirmation dialog and the biometric are the same gesture, which
is the nicest version of this feature — the key cannot leave the phone, and
every use is a fingerprint.

**3. Browser.** WebAuthn produces exactly the signature format OpenSSH's
`sk-ecdsa-sha2-nistp256@openssh.com` expects, so a passkey becomes an SSH key
with a touch. Highest effort — the `sk` wire format, the signature counter and
credential discovery all need care — and worth doing last, once the router is
proven by the other two.

## Open questions

- **Holder handoff.** If the phone disconnects mid-session, does the browser
  silently take over? Probably not: an explicit claim, and in-flight requests
  fail rather than being re-routed to a different key.
- **Which key.** `REQUEST_IDENTITIES` may return several; a tab-level allowlist
  of fingerprints would let one tab use the deploy key and another the personal
  one, from the same holder.
- **Desktop GUI.** Per-tab ssh-agent is headless-only today
  (`501` from the GUI route). The router has the same constraint until the
  GUI's spawn path learns to inject `SSH_AUTH_SOCK`.
- Does the sidecar want the *reverse* too — a local tab borrowing the remote's
  agent? Symmetric on the wire, but a much worse idea, and out of scope.
