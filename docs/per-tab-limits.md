# Per-tab resource limits (cgroup v2)

Cap a single tab's memory / CPU / task count so one runaway shell (a
build that leaks, an agent that balloons) can't OOM the whole machine or
take the other tabs down with it. Each tab's process tree is moved into
its own cgroup v2 under our delegated subtree; the tab that exceeds
`memory.max` is OOM-killed **alone**, leaving the app and the other tabs
alive.

Implemented in [`src/cgroup.rs`](../src/cgroup.rs); wired into both the
GUI (`src/app.rs`) and the headless daemon (`src/headless.rs`).

## Configure the ceilings

In `preferences.json` (see `platform::config_dir()` —
`~/.config/tab-atelier/preferences.json` on Linux):

```json
{
  "default_tab_limits": { "memory_max": "8G", "tasks_max": 512 }
}
```

- `memory_max` — hard `memory.max` per tab (e.g. `"8G"`, `"512M"`). A tab
  reaching it is OOM-killed inside its own cgroup, nothing else.
- `cpu_quota_percent` — `cpu.max` quota (100 = one core, 250 = 2.5 cores).
- `tasks_max` — `pids.max`, an anti-fork-bomb ceiling.

A tab can override any axis via its own `limits` (persisted in
`tabs.json`); unset axes fall back to `default_tab_limits`. All unset (the
default) keeps tabs unlimited, exactly as before.

## Requirement: a delegated cgroup subtree

Limits only take effect when systemd has delegated our cgroup subtree —
otherwise `cgroup::init` detects the non-writable subtree and silently
disables limiting (tabs still spawn, just unlimited).

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
