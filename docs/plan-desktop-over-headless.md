# Plan: desktop GUI as a client of the headless daemon (+ denied-domains UX)

Status: planning (drafted while offline). No code yet.

## Why
- One owner of PTYs/tabs → kills the `TerminalView` ⇄ `HeadlessTab` + dual-drain
  duplication.
- Desktop tabs get **real egress enforcement** (the daemon holds `CAP_NET_ADMIN`;
  the unprivileged GUI never can).
- Tabs **survive a GUI crash/restart** (tmux-style).

## Key realization
Most of this already exists: **remote attach** (`remote.rs` + the `/stream`/`/input`
WebSocket endpoints + the PTY ring) already streams and renders a headless
daemon's tabs over WS. So this is mostly "point the GUI at a *local* daemon and
stop owning PTYs in-process," not a new transport.

## Transport — WebSocket framing; which stream?
WS is already the protocol. Two phases:
1. **Phase 1 — WS over loopback TCP.** Reuse remote-attach pointed at
   `127.0.0.1:7890`. Auth = master token (read the daemon's `api.token`).
   Almost no new transport code.
2. **Phase 2 — WS over Unix domain socket** (`/run/tab-atelier/gui.sock`). Same WS
   message protocol over a UDS: lower latency, and auth becomes **file
   permissions** (drop the on-disk token for local use), no TCP port exposed.

## Rendering / input
- Keep the gpui terminal renderer; feed it from `/stream?since=N` into a *local*
  alacritty `Term` mirror. Background tabs render from the cheap `/tabs` snapshot
  (preview/LED) and only attach their stream when focused.
- Input: keystroke → WS `/input` → daemon PTY → echo → ring → `/stream` → GUI
  parser → paint. ~2 loopback hops, sub-ms, imperceptible. UDS shaves more.

## Daemon lifecycle (the real decision)
- (a) GUI spawns a child daemon: self-contained but **no CAP_NET_ADMIN** (no
  desktop egress enforcement) unless the binary is `setcap`'d; tabs die with GUI.
- (b) **Attach to a per-user privileged service** (recommended): `User=<you>` +
  `AmbientCapabilities=CAP_NET_ADMIN`, FS hardening relaxed so your interactive
  tabs see your real `$HOME`/filesystem. Gives egress enforcement + survive-
  restart. GUI auto-starts it (`systemctl --user` / socket activation) if down.
  Cost: it's a per-user privileged daemon, not a locked-down system one.

## What's deleted / moves
- `terminal.rs`: PTY ownership → ring-fed renderer (keep paint, drop `tty::new` +
  event loop).
- `app.rs`: tab spawn/respawn/persist/drain → deleted on the GUI side (daemon owns
  it). Removes the big duplication.
- GUI keeps: rendering, input, tab strip, menus, preferences, theme/OSC replies.

## Migration (each step shippable)
1. GUI "use local daemon" mode: attach to `127.0.0.1`, render those tabs alongside
   in-process ones.
2. Flip default: new tabs created on the daemon, rendered via the ring.
3. Remove in-process PTY ownership + the duplicated lifecycle.
4. Phase 2: WS-over-UDS + file-perm auth.

## Risks / open decisions
- Latency under gpui frame stalls (local echo prediction probably unneeded).
- Bootstrap/permissions of the per-user privileged daemon.
- Protocol-version handshake on `/tabs` (GUI↔daemon skew).
- Decision: spawn-child vs attach-system (recommend attach-system).

---

# Show the user the DENIED domains

Goal: make "an agent tried to reach X and the allowlist blocked it" visible — so
the user knows *why* a request failed and can add X to the allowlist if intended.

Already available (headless): denied queries are logged by `net_resolver`
(`DnsEntry { allowed: false }`), surfaced on `/tabs` as `dns`, and printed by
`net-dns` as `✗ DENIED`.

Surfaces to add, best-first:

1. **Inline tab notice (most visible).** When the resolver denies a query, have
   the daemon write a dim one-liner into that tab's PTY ring:
   `⚠ tab-atelier: blocked DNS for evil.com (not on allowlist)`. The user/agent
   sees it *exactly where the failure happened*. Risk: corrupting a full-screen
   TUI — so make it **opt-in** (preference) and/or rate-limited + only when the
   tab isn't in alt-screen mode. The resolver already has the domain; it needs a
   handle to the tab's ring (it runs in the daemon, so this is wiring, not new
   transport).
2. **Desktop "Network" panel / right-click → "DNS entries"** (after the
   desktop-over-headless work): list the tab's resolver log with denied rows
   highlighted (red) and an "Allow" action that calls `net-allow --add --domain`.
   This is the richest surface and closes the loop (see → one click → allowed).
3. **Share-link web viewer banner.** A small collapsible "Blocked: a.com, b.com"
   strip on the xterm.js page. Needs a share-token-scoped `GET
   /tabs/by-id/{id}/dns` (today `dns` is only on master-token `/tabs`).
4. **CLI (done):** `net-dns [tab]` shows ✓/✗ DENIED + IPs. Add `--denied` to filter
   to just blocked, for quick "what did it try" checks.

Recommended order: (4 `--denied`, cheap) → (1 inline notice, opt-in) → (2 desktop
panel, with the desktop-over-headless work) → (3 web banner).

Data note: the denied list lives in `ResolverHandle`'s in-memory log (bounded,
de-duped). For the inline notice it's pushed at denial time; for the panels it's
read from `/tabs` `dns`.
