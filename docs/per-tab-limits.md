# Per-tab resource limits (cgroup v2)

Cap a single tab's memory / CPU / task count so one runaway shell (a
build that leaks, an agent that balloons) can't OOM the whole machine or
take the other tabs down with it. Each tab's process tree is moved into
its own cgroup v2 under our delegated subtree; the tab that exceeds
`memory.max` is OOM-killed **alone**, leaving the app and the other tabs
alive.

Implemented in [`src/cgroup.rs`](../src/cgroup.rs); wired **identically**
into both the GUI (`src/app.rs`) and the headless daemon
(`src/headless.rs`) — same `init` + `apply`/`reapply`, so a limit behaves
the same whichever binary owns the tab.

## Configure the defaults

In `preferences.json` (see `platform::config_dir()` —
`~/.config/tab-atelier/preferences.json` on Linux):

```json
{
  "default_tab_limits": { "memory_max": "8G", "tasks_max": 512 }
}
```

- `memory_max` — hard `memory.max` per tab (e.g. `"8G"`, `"512M"`, or a
  bare byte count). A tab reaching it is OOM-killed inside its own cgroup,
  nothing else. `K`/`M`/`G`/`T` are 1024-based.
- `cpu_quota_percent` — `cpu.max` quota (100 = one core, 250 = 2.5 cores).
- `tasks_max` — `pids.max`, an anti-fork-bomb ceiling.

A tab can override any axis with its own `limits` (persisted in
`tabs.json`); unset axes fall back to `default_tab_limits`. All unset (the
default) keeps tabs unlimited, exactly as before.

## Set limits on a live tab (CLI)

Both binaries expose a `limit` subcommand that caps a *running* tab over
the local API — no restart, no editing files. Each flag sets one axis;
axes you don't pass keep their current value.

```console
# GUI build (works alongside the running desktop app)
$ tab-atelier limit 3 --memory 8G --cpu 250 --tasks 512
limits set for tab 3: memory=8G, cpu=250%, tasks=512 (applies on the next drain tick)

# headless daemon — identical surface
$ tab-atelier-headless limit 3 --memory 4G

# target by stable UUID instead of index
$ tab-atelier limit 6e1c… --cpu 100

# lift every limit (back to unlimited)
$ tab-atelier limit 3 --clear
limits cleared for tab 3 (applies on the next drain tick)
```

The change is queued and applied on the owner's next drain tick (well
under a second), re-writing the tab's cgroup in place — a running tab is
capped, or freed, without respawning its shell.

## Set limits on a live tab (API)

`POST /tabs/by-id/<uuid>/limits` (or `/tabs/<index>/limits`). Body fields
are all optional; send the axes you want to change, or `{"clear": true}`
to lift everything:

```console
$ curl -X POST \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"memory_max":"8G","cpu_quota_percent":250,"tasks_max":512}' \
    http://127.0.0.1:7890/tabs/by-id/<uuid>/limits
{"queued":"limits"}
```

| Field               | Type        | Maps to             |
| ------------------- | ----------- | ------------------- |
| `memory_max`        | string      | `memory.max`        |
| `cpu_quota_percent` | number      | `cpu.max` quota     |
| `tasks_max`         | number      | `pids.max`          |
| `clear`             | bool        | reset every axis    |

A bad `memory_max` (not a byte count / `K`/`M`/`G`/`T` value) is rejected
with `400` up front rather than silently no-op'ing at cgroup-write time.
`clear` is mutually exclusive with the axis fields.

## Requirement: a delegated cgroup subtree

Limits only take effect when systemd has delegated our cgroup subtree —
otherwise `cgroup::init` detects the non-writable subtree and silently
disables limiting (tabs still spawn, just unlimited, and the CLI/API
changes queue harmlessly with no effect).

- **Headless** — `tab-atelier-headless.service` already ships
  `Delegate=yes`. Nothing to do.
- **GUI** — the desktop launches the app in a transient
  `app-tab-atelier-*.scope` that is **not** delegated by default. Launch
  it through a delegated scope instead. In the `.desktop` file (user copy
  at `~/.local/share/applications/` or `~/.config/autostart/`):

  ```ini
  Exec=systemd-run --user --scope -p Delegate=yes \
       -p MemoryHigh=20G -p MemoryMax=24G /usr/bin/tab-atelier
  ```

  `Delegate=yes` unlocks the per-tab limits above. The optional
  `MemoryHigh`/`MemoryMax` on the app scope are a whole-app belt on top of
  the per-tab suspenders — they bound tab-atelier's total footprint so a
  runaway can't starve the rest of the desktop.

Verify delegation took: after launch, the app's own cgroup
(`cat /proc/$(pgrep -x tab-atelier)/cgroup`) should contain a
`supervisor/` leaf and `tab-<id>/` children with your `memory.max`
written in.
