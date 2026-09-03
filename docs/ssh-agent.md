# Per-tab ssh-agent (headless daemon)

Give a single tab its own `ssh-agent` so different tabs can hold different
SSH identities — or none — without cross-contamination. By default every
tab shares the daemon's ambient environment, so any tab doing `git`/`ssh`
work reaches the same (or no) agent. With this feature the daemon owns a
dedicated agent per opted-in tab and injects its `SSH_AUTH_SOCK` into that
tab's shell.

A proposal to invert this — the **viewer** holds the key and the daemon only
routes agent traffic to it — is sketched in
[ssh-agent-proxy.md](./ssh-agent-proxy.md).

Managed feature: the daemon owns the agent's lifecycle (spawn, optional
key load, reap). Headless-only — the desktop GUI's spawn path is not wired
for it, so the GUI's `POST /tabs/by-id/{id}/ssh-agent` route refuses with
`501`. Implemented in [`src/ssh_agent.rs`](../src/ssh_agent.rs); spawn
wiring in [`src/headless.rs`](../src/headless.rs).

## Use it

```sh
# Enable an empty agent for a tab (ssh-add your keys inside the tab):
tab-atelier-headless ssh-agent 2

# Enable + auto-load a passphrase-less key:
tab-atelier-headless ssh-agent 2 --key /var/lib/tab-atelier/keys/id_ed25519

# Disable and reap the agent:
tab-atelier-headless ssh-agent 2 --off
```

`<tab>` is a tab index or UUID. Enabling/disabling **respawns the tab's
shell** (see below), so a running `claude` is killed and auto-resumed —
the same model as `net-off`/`net-on`.

Inside the tab afterwards:

```sh
echo "$SSH_AUTH_SOCK"   # points at this tab's private agent socket
ssh-add -l              # lists the tab's loaded keys
```

## Behaviour & constraints

- **Respawn on toggle.** `SSH_AUTH_SOCK` can only enter a process at spawn,
  so enabling/disabling the agent restarts the PTY. Scrollback is fed back;
  durable state (name, tokens, agent session) carries across.
- **Unencrypted keys only** are auto-loaded. `--key` runs `ssh-add` with
  stdin closed, so an encrypted key fails fast rather than hanging; load
  encrypted keys yourself with `ssh-add` inside the tab.
- **The agent survives an unrelated respawn.** It is a daemon child (not in
  the tab's cgroup), so a net toggle or auto-resume `cgroup.kill` does not
  touch it — keys loaded earlier stay loaded. `systemctl restart` kills the
  whole service cgroup (agents included) and the persisted config
  re-provisions fresh agents on boot, so no orphans accumulate.
- **Persisted.** The config lives in `tabs.json`, re-applied on boot /
  auto-resume.

### Key paths under the hardened unit

The shipped `tab-atelier-headless.service` runs with a private mount
namespace (`TemporaryFileSystem=/`, `ProtectHome=true`). A `--key` path
must therefore live **inside the daemon's namespace** — under the unit's
`StateDirectory` (`/var/lib/tab-atelier`), `/srv`, or a path you bind in
via `systemctl edit tab-atelier-headless`. A key under a real `/home/...`
is invisible to the daemon and the auto-load silently no-ops (logged as a
warning). Keep per-tab keys under `/var/lib/tab-atelier/keys/` owned by the
`tab-atelier` service user, mode `0600`.

## Degradation

Best-effort throughout: a missing `ssh-agent`/`ssh-add` binary
(`openssh-client` is a deb *Recommends*, not a hard dependency), an
unwritable socket directory, or an `ssh-add` failure never kills a tab —
it just spawns without a per-tab `SSH_AUTH_SOCK`, and a one-line warning is
logged.
