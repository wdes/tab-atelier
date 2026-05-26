# Tab Atelier

[![Build](https://github.com/wdes/tab-atelier/actions/workflows/build.yml/badge.svg)](https://github.com/wdes/tab-atelier/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/wdes/tab-atelier/branch/main/graph/badge.svg)](https://codecov.io/gh/wdes/tab-atelier)
![Unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-brightgreen.svg)

A Guake-style drop-down terminal emulator for Linux (X11), built with Rust using [alacritty_terminal](https://crates.io/crates/alacritty_terminal), [gpui](https://crates.io/crates/gpui) (Zed's GPU-accelerated UI framework), and [wattaouille](https://crates.io/crates/wattaouille) for power monitoring.

![Tab Atelier screenshot](.github/docs/screenshot.webp)

![Low battery warning](.github/docs/screenshot-low-battery.webp)

## Features

**Terminal**
- Drop-down terminal toggled with global hotkeys (default: **`** and **XF86Calculator**, customizable in preferences)
- Full terminal emulation via alacritty_terminal (colors, scrollback, bracketed paste, ...)
- GPU-accelerated rendering via gpui
- Text selection with mouse, copy/paste from context menu
- Clickable URLs and file paths detected in terminal output
- Reset input & color for misbehaving programs

**Tabs**
- Multiple tabs with drag-and-drop reordering
- Double-click to rename, right-click context menu
- "Copy path" right-click entry copies the tab's working directory to the clipboard
- **Ctrl+Shift+T** to open a new tab (inherits working directory)
- **Alt+Tab** to cycle between tabs
- Shell exit detection with close/respawn confirmation
- Per-tab **agent state LED** to the left of the tab name (thinking / waiting / error), driven by an in-tab CLI (see [Agent state](#agent-state))

**Session**
- Tabs, working directories, and full terminal output persisted across restarts
- Active tab selection restored on startup
- **Agent auto-resume**: tabs that were running `catbus-agent` or `claude` at last save reopen with `catbus-agent --resume <uuid>` / `claude --resume <uuid>` typed into the freshly-spawned shell

**Preferences**
- Theme selection (Dark, Tomorrow Night Blue)
- Window opacity (1%-100% slider)
- Language (English, French)
- Configurable toggle hotkeys (press any key to register, applied immediately)
- Configurable browser and code editor for opening links

**Monitoring**
- Per-tab CPU usage, power draw (watts), energy consumption (Wh), and uptime
- Low battery warning with visual indicator

**Integration**
- HTTP API (port 7890) + TLS variant (port 7891) with token auth and QR code for remote tab management from a phone
- `tab-atelier set-status` CLI for in-tab tools (agents, hooks, scripts) to publish thinking/waiting/error state to the desktop LED
- Optional `happier-bridge` (off by default) republishes tab state into a bundled `happier-relay` so the [happier](https://github.com/maximegris/happier) mobile companion can view sessions, send keystrokes, and see per-tab agent state
- Wakatime time tracking (reads API key from Zed settings)
- Screenshots (per-tab or full app) saved as BMP

## Installation

```sh
cargo build --release
# Binary at target/release/tab-atelier
```

Requires Rust 2024 edition (rustc 1.92+).

### Debian package

```sh
cargo deb
sudo apt install ./target/debian/tab-atelier_*.deb
```

The `.deb` lays out the following under FHS-standard paths:

| Path | Permission | Contents |
|---|---|---|
| `/usr/bin/tab-atelier` | `0755` | The binary |
| `/usr/share/applications/tab-atelier.desktop` | `0644` | Desktop entry (registers Tab Atelier in app launchers) |
| `/usr/share/icons/hicolor/scalable/apps/tab-atelier.svg` | `0644` | App icon (scalable SVG, picked up by the desktop environment via the `Icon=tab-atelier` line in the .desktop) |
| `/usr/share/doc/tab-atelier/` | `0644` | `README.md`, `LICENSE`, `copyright` |

**`conffiles` (per [debian-policy §10.7.2](https://www.debian.org/doc/debian-policy/ch-files.html#behavior)):** none. tab-atelier ships no system-wide configuration in `/etc`, so dpkg has no files to track between upgrades. All user-modifiable state — preferences, tab list, scrollback, uptime, energy, single-instance lock — lives under the user's `$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` (see [State](#state) below). The package's `conf-files = []` in `Cargo.toml` records this intentionally.

**`dirs` (per [debian-policy §10.5](https://www.debian.org/doc/debian-policy/ch-files.html#permissions-and-owners)):** the package creates only what it installs (under `/usr/bin`, `/usr/share/applications`, `/usr/share/icons/hicolor/scalable/apps`, `/usr/share/doc/tab-atelier`). It does **not** pre-create any directory under `/etc` or `/var`. The per-user `~/.config/tab-atelier/` and `~/.local/{,state/}tab-atelier/` directories are created lazily by the running application on first save — dpkg never touches them, so a `dpkg --purge` leaves them in place for the user to remove manually if desired.

## Running

```sh
tab-atelier               # normal mode
tab-atelier --read-only   # second instance, no writes
```

A normal launch acquires a single-instance lock on `~/.local/state/tab-atelier/tab-atelier.lock` and exits if another normal instance is already running — concurrent writers would race each other and produce inconsistent state files.

`--read-only` skips the lock so any number of read-only instances can run alongside the primary one. In that mode tab-atelier never writes anything: no `tabs.json` rewrites, no per-tab output / uptime / energy files, no preference saves, no rename-time file moves. The preferences "Save" button is visually disabled. Useful for snapshotting the running workspace from a script or for poking around without disturbing live state.

## Configuration

### Font settings

Tab Atelier reads font configuration from your Zed editor settings:

**File:** `$XDG_CONFIG_HOME/zed/settings.json` (defaults to `~/.config/zed/settings.json`)

| Setting              | Description                  | Default      |
|----------------------|------------------------------|--------------|
| `ui_font_family`     | Terminal font family         | `monospace`  |
| `ui_font_weight`     | Font weight (100-900)        | `400`        |
| `ui_font_size`       | Font size in pixels          | `16`         |
| `buffer_font_size`   | Fallback if no ui_font_size  | `16`         |
| `scroll_sensitivity` | Scroll speed multiplier      | `1.0`        |

**Font weight tip.** When `ui_font_weight` is something other than a multiple of 100 that maps to a real static face (e.g. `250`), use a **variable** font family — fontconfig otherwise picks the closest static face per glyph and rarely-used codepoints (`€`, `—`, …) can end up in a different face than the digits next to them, which reads as "uneven bold". `scripts/install-monaspace.sh` installs Monaspace v1.400 (variable build) into `~/.local/share/fonts/Monaspace/`; pair it with `"ui_font_family": "Monaspace Neon Var"`.

### Preferences

In-app preferences (theme, opacity, language, browser, code editor) are stored in:

**File:** `$XDG_CONFIG_HOME/tab-atelier/preferences.json` (defaults to `~/.config/tab-atelier/preferences.json`)

### State

Tab Atelier splits persisted state across four files to keep a bad write to any one piece from corrupting the rest. Each file is written atomically (`.tmp` + `fsync` + rename) with rotated backups (`.bak`, `.bak.1`, `.bak.2`).

| Data | Path | Notes |
|---|---|---|
| Tab list, working directories, active index | `~/.local/tab-atelier/tabs.json` | Tiny, rewritten on every persist tick (~2 s) |
| Per-tab terminal scrollback | `~/.local/state/tab-atelier/output_tab-<sanitized>-<crc32>.json` | One file per tab. Rewritten on every persist tick |
| Per-tab uptime (active seconds) | `~/.local/state/tab-atelier/uptime_tab-<sanitized>-<crc32>.json` | One file per tab. **Throttled to once every 30 s**; final value flushed on shutdown |
| Per-tab energy (Wh) | `~/.local/state/tab-atelier/power_tab-<sanitized>-<crc32>.json` | One file per tab. **Throttled by delta (≥ 0.1 Wh consumed)**; final value flushed on shutdown |
| Single-instance lock | `~/.local/state/tab-atelier/tab-atelier.lock` | Empty file. Held via `flock(2)`; released automatically by the kernel on process exit (including crashes), so no manual cleanup needed |

Tab filename = sanitized tab name (non-`[A-Za-z0-9._-]` → `_`) plus an 8-hex-digit CRC32 of the original name, so two tabs whose sanitized forms collide (e.g. `foo/bar` and `foo_bar`) still land in distinct files. Renaming a tab in the UI moves all four files (output, uptime, power, plus their `.bak`s) to the new name's slot so history isn't orphaned.

## Power monitoring

On Intel systems with readable RAPL counters, each tab shows its estimated power usage in the right-click context menu. The estimate uses the same technique as [wattaouille](https://github.com/wdes/wattaouille): `per-tab watts = package watts * (tab CPU jiffies / total system jiffies)`. When RAPL is not available, only CPU percentage is shown.

### Making RAPL readable

Since CVE-2020-8694 (PLATYPUS side-channel) the kernel ships `/sys/class/powercap/intel-rapl/intel-rapl:*/energy_uj` as `mode 400, owned by root`, so a regular user — including the one running tab-atelier — gets `Permission denied`. Symptom: the watts column on every tab card is empty, the stats popover shows CPU% only, and `~/.local/state/tab-atelier/power_tab-*.json` files never get created.

**One-shot for the current boot:**

```sh
sudo chmod -R g+r,o+r /sys/devices/virtual/powercap/intel-rapl
```

**Persistent (every boot)** — drop a udev rule:

```sh
echo 'SUBSYSTEM=="powercap", ACTION=="add", RUN+="/bin/chmod -R g+r,o+r /sys/devices/virtual/powercap/intel-rapl"' | sudo tee /etc/udev/rules.d/99-rapl.rules
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=powercap
```

After either, **restart tab-atelier**. `PowerSensor::detect` runs once at startup, so a mid-session permission fix is only picked up by the next launch.

**What are jiffies?** Jiffies are the Linux kernel's internal time-keeping unit — a counter that increments at a fixed rate (typically 100, 250, or 1000 Hz depending on `CONFIG_HZ`). Each tick, the kernel records CPU time consumed by every process. Per-process jiffies are read from `/proc/[pid]/stat` and total system jiffies from `/proc/stat`. The ratio between them gives the fraction of CPU a tab's shell used, which is multiplied by package power (from Intel RAPL) to estimate per-tab wattage.

## HTTP API

Tab Atelier exposes tab state on `http://<local-ip>:7890` as JSON (and `https://<local-ip>:7891` over TLS with a self-signed cert auto-generated under `~/.local/state/tab-atelier/tls.{crt,key}`). Access requires a bearer token, shown via a QR code in the right-click menu ("Remote control"). The response includes tab names, working directories, active tab index, and per-tab power stats.

Selected routes:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/tabs` | List tabs (cwd, name, preview, watts, cpu, uptime) |
| `GET` | `/tabs/{idx}/output` | Tab scrollback (supports `?since=N&crc=…` for delta patching) |
| `POST` | `/tabs/{idx}/input` | Send raw bytes to the tab's PTY |
| `POST` | `/tabs/{idx}/activate` | Switch to a tab |
| `POST` | `/tabs/{idx}/rename` | Rename a tab |
| `POST` | `/tabs/by-id/{tab_id}/status` | Publish agent state — see [Agent state](#agent-state) |
| `DELETE` | `/tabs/{idx}` | Close a tab |

Bind addresses for both listeners are configurable in preferences (`api_addr`, `api_tls_addr`); pass `--read-only` to launch a second instance that serves the API but refuses every mutating verb.

## Agent state

Each tab carries an optional **agent state** rendered as a small colored LED to the left of the tab name:

| State | LED |
|---|---|
| `thinking` | steady cyan |
| `waiting` | amber that alternates with grey every 500 ms (same cadence as the low-battery indicator) — easy to spot from across the room without strobing the eye |
| `error` | steady red |
| _none, but a session is attached_ | steady grey — "agent CLI lives here, no recent activity" |
| _none, no session_ | hidden |

The grey-when-idle behaviour means the LED stays visible for the entire life of an attached agent session: once your shell sends a `--session <uuid>` it sticks until the session genuinely ends. Claude Code's `SessionEnd` hook (or a manual `tab-atelier set-status idle`) wipes both the transient state and the durable session attachment so the LED disappears.

State is stored in RAM only (the durable session id, agent kind, and plan-mode flag are persisted to `tabs.json` for auto-resume).

Every PTY tab-atelier spawns gets three env vars so in-tab tools can publish state without configuration:

| Variable | Value |
|---|---|
| `_TAB_ID` | Stable per-tab UUID. Survives renames. |
| `TAB_ATELIER_API_URL` | `http://127.0.0.1:<api_port>` |
| `TAB_ATELIER_API_TOKEN` | Same token shown by the "Remote control" QR code |

### `tab-atelier set-status` CLI

```sh
tab-atelier set-status <state> [--label <hint>] \
                                [--session <uuid>] \
                                [--kind <catbus|claude|…>] \
                                [--plan|--no-plan]

# state: idle | thinking | waiting | error
```

The CLI silently exits 0 when `_TAB_ID` is unset (i.e. invoked outside a tab), so it is safe to call unconditionally from `.bashrc` snippets, agents, or build hooks.

`catbus-agent` calls this internally at each lifecycle point. To get the same LED behaviour out of an external Claude Code session, configure a hook in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "matcher": "", "hooks": [{ "type": "command",
        "command": "bash -c 'sid=$(jq -r .session_id); tab-atelier set-status thinking --kind claude --session \"$sid\"' >/dev/null 2>&1" }] }
    ],
    "Stop": [
      { "matcher": "", "hooks": [{ "type": "command",
        "command": "bash -c 'sid=$(jq -r .session_id); tab-atelier set-status waiting --kind claude --session \"$sid\"' >/dev/null 2>&1" }] }
    ],
    "PermissionRequest": [
      { "matcher": "", "hooks": [{ "type": "command",
        "command": "bash -c 'sid=$(jq -r .session_id); tab-atelier set-status waiting --label permission --kind claude --session \"$sid\"' >/dev/null 2>&1" }] }
    ],
    "StopFailure": [
      { "matcher": "", "hooks": [{ "type": "command",
        "command": "bash -c 'sid=$(jq -r .session_id); tab-atelier set-status error --kind claude --session \"$sid\"' >/dev/null 2>&1" }] }
    ],
    "SessionEnd": [
      { "matcher": "", "hooks": [{ "type": "command",
        "command": "bash -c 'sid=$(jq -r .session_id); tab-atelier set-status idle --kind claude --session \"$sid\"' >/dev/null 2>&1" }] }
    ]
  }
}
```

| Event | State | Why |
|---|---|---|
| `SessionStart` | `waiting` (label `session`) | Claude attached to the tab — LED appears immediately, not only at first prompt |
| `UserPromptSubmit` | `thinking` | You sent a prompt — work has started |
| `PreToolUse` | `thinking` (label `tool`) | Refreshes the LED on every tool call so a long turn never gets reclaimed by the staleness sweep |
| `Stop` | `waiting` | Claude finished its turn, awaiting next prompt |
| `PermissionRequest` | `waiting` (label `permission`) | Claude paused for tool/file approval — needs you |
| `StopFailure` | `error` | Turn aborted (API error, crash) |
| `SessionEnd` | `idle` | Session terminated — clear the LED |

### Auto-resume on restart

When a tab carries both `agent_kind` and `agent_session_id` in `tabs.json`, the restored tab — about 500 ms after its shell has come up — receives a Ctrl-U followed by the appropriate resume command:

| `agent_kind` | Injected command |
|---|---|
| `catbus` | `catbus-agent --resume <uuid>` (plus ` --plan` if the agent was in plan mode at save time) |
| `claude` | `claude --resume <uuid>` |
| anything else | no-op |

If the agent CLI is no longer on `PATH`, the shell prints `command not found` and the tab is otherwise unaffected.

## Mobile companion (happier-bridge)

The `happier-bridge` feature (off by default — enabled in the bundled `.deb`) republishes each tab as an artifact in a local **happier-relay** instance, so the [happier](https://github.com/maximegris/happier) mobile/web client can browse sessions, view scrollback, type into the PTY, and see per-tab agent state. The relay binds on port 7892 with TLS, sharing the same self-signed cert as the API TLS listener.

```sh
cargo build --release --features happier-bridge
```

## Wakatime

Wakatime integration is automatic if your Zed settings contain `wakatime.settings.api-key`. Heartbeats are sent with project detection (walks up to find `.git`).

## Logging

Control log output with the `RUST_LOG` environment variable:

```sh
RUST_LOG=tab_atelier=info cargo run    # Startup, restore, API
RUST_LOG=tab_atelier=debug cargo run   # Verbose
```

## Testing

```sh
cargo test
cargo clippy
```

After a fresh clone, opt-in to the repo's pre-commit hook so CI's
`Check formatting` step can't fail on a freshly-pushed commit:

```sh
git config core.hooksPath .githooks
```

The hook runs `cargo fmt -- --check` and aborts the commit (with the
offending diff) when the tree drifts from rustfmt. Pass `--no-verify`
to skip for a one-off WIP commit.

## License

MPL-2.0

Terminal rendering patterns based on [Zed](https://github.com/zed-industries/zed) (Apache-2.0 / GPL-3.0).
