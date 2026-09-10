# Hot swap — upgrade without losing a single tab

`tab-atelier upgrade` (or `POST /upgrade`) replaces the **running**
binary with the one currently installed at its path while every tab's
shell — and whatever is running inside it (a `claude` session, a build,
an ssh connection) — keeps running, unaware anything happened.

## Using it

```sh
# 1. Install the new version over the old one (any of these):
sudo apt install ./tab-atelier_0.6.0_amd64.deb
sudo cp target/release/tab-atelier /usr/bin/tab-atelier

# 2. Ask the running instance to swap itself:
tab-atelier upgrade                 # desktop GUI
tab-atelier-headless upgrade        # headless daemon
# or: curl -X POST http://127.0.0.1:7890/upgrade \
#          -H "Authorization: Bearer $(tab-atelier token)"
```

The process re-execs within a couple of seconds (its next owner-loop
tick). The GUI window closes and reopens on the new version; the
headless daemon's API drops for a moment and re-binds. Tabs, shells,
agents, cgroups, and nftables egress rules all survive.

## How it works

A normal restart forks fresh shells and replays saved output text. The
hot swap instead `exec()`s the new binary **in place** (`src/hotswap.rs`):

1. **Freeze.** `PtyTap::read` starts reporting `WouldBlock`, parking
   every PTY reader. Bytes the shells emit from now on wait in the
   kernel PTY buffers and are read by the new binary — nothing is lost.
2. **Flush.** The usual quit-path persistence runs (tabs.json, per-tab
   output/uptime/energy), so the new binary restores names, cwds, grid
   contents, and scrollback through the existing restore code.
3. **Handoff manifest.** For each live tab, a dup of the PTY **master**
   fd gets its `CLOEXEC` flag cleared, the raw `PtyRing` bytes are
   written to a sidecar (so web-viewer scrollback survives), and
   `(tab id, fd number, shell pid)` is recorded in
   `<state>/tab-atelier/handoff.json`.
4. **exec.** The process replaces itself with the binary at its own
   path (`/proc/self/exe`, with dpkg's ` (deleted)` suffix stripped),
   passing `--handoff <manifest>` on argv. Because `exec` keeps the
   pid, the tab shells remain our **children** — process groups,
   controlling TTYs, and SIGCHLD reaping are all untouched. If the exec
   fails, everything rolls back and the old binary keeps running.
5. **Adopt.** At boot the new binary validates the manifest (schema
   version + writer pid must equal its own pid — after exec they match;
   a stale manifest from a crashed swap never can) and stashes the fds
   in a registry keyed by tab id. The tab restore path claims entries
   from that registry and wraps each fd in an `AdoptedPty` — a drop-in
   for alacritty's Unix `Pty` (same poller tokens, SIGCHLD pipe, and
   `waitpid`-based exit detection) — instead of forking a shell.
   Unclaimed fds are closed once every tab has spawned.

## What deliberately does NOT happen for adopted tabs

- **No agent auto-resume.** The agent is still running in the adopted
  shell; typing `claude --resume …` would double-launch the session.
- **No net-off respawn (GUI).** The adopted shell is still inside the
  bubblewrap netns the previous run put it in.
- **No nftables teardown/re-apply (headless).** The tab's table and
  cgroup are kernel state that survived the exec; re-applying would
  open a brief unconfined window for the running shell. Only the
  daemon-side gating DNS resolver (a thread that died with the old
  process) is respawned for domain-allowlist tabs.
- **No orphan reaping.** The swap deletes the agent reaper's provenance
  record (clean-handover semantics) and the headless cgroup reaper
  skips adopted tabs — both would otherwise SIGKILL exactly the
  processes the handoff kept alive.

## Failure behaviour

- Shell died mid-swap → its manifest entry fails the `waitpid` probe
  and the tab falls back to a normal fresh fork (with the carried ring
  bytes still seeding the scrollback above it).
- `exec` failed (binary missing/corrupt) → `CLOEXEC` is restored, the
  manifest is removed, readers unfreeze, and the old binary keeps
  running; the error lands in the log and the endpoint caller's next
  poll.
- Downgrading to a pre-hot-swap binary → the old binary ignores
  `--handoff`, so tabs respawn fresh (a normal restart) and the handed
  fds leak until the shells are HUP'd. Upgrade forward instead.

## Limits

- Unix only (Windows ConPTY handles can't cross an exec; the endpoint
  answers 501 there).
- WebSocket viewers and `remote attach` clients are disconnected by the
  exec and must reconnect — their scrollback survives via the carried
  ring bytes.
- The single-instance lock is dropped and re-acquired across the exec
  (std opens it `CLOEXEC`); a different instance racing for it in that
  window loses the tabs to the "already running" check — in practice
  unobservable.
