# Tab Atelier

A Guake-style drop-down terminal emulator for Linux (X11), built with Rust using [alacritty_terminal](https://crates.io/crates/alacritty_terminal) and [gpui](https://crates.io/crates/gpui) (Zed's GPU-accelerated UI framework).

## Features

- Drop-down terminal toggled with global F12 hotkey
- Multiple tabs with right-click context menu
- Tab rename, copy/paste, close, close all
- Session persistence (tabs, working directories, and terminal output saved across restarts)
- Text selection with mouse
- Scrollback history with ANSI color preservation
- Shell exit detection with close/respawn confirmation
- Per-tab power usage and uptime display (Intel RAPL, same technique as [wattaouille](https://github.com/wdes/wattaouille))
- HTTP API with token auth and QR code for remote tab management
- Wakatime time tracking (reads API key from Zed settings)
- Reset input & color mode for misbehaving programs

## Settings

Tab Atelier reads font configuration from your Zed editor settings file:

**File:** `$XDG_CONFIG_HOME/zed/settings.json` (defaults to `~/.config/zed/settings.json`)

| Setting              | Description                  | Default      |
|----------------------|------------------------------|--------------|
| `ui_font_family`     | Terminal font family         | `monospace`  |
| `ui_font_weight`     | Font weight (100-900)        | `400`        |
| `ui_font_size`       | Font size in pixels          | `16`         |
| `buffer_font_size`   | Fallback if no ui_font_size  | `16`         |
| `scroll_sensitivity` | Scroll speed multiplier      | `1.0`        |

### Logging

Tab Atelier uses `env_logger`. Control log output with the `RUST_LOG` environment variable:

```sh
# Show all logs
RUST_LOG=tab_atelier=debug cargo run

# Show only warnings and errors
RUST_LOG=tab_atelier=warn cargo run

# Show info-level logs (startup, restore, API)
RUST_LOG=tab_atelier=info cargo run

# Show logs from all crates (verbose)
RUST_LOG=debug cargo run

# Filter specific modules
RUST_LOG=tab_atelier::api=debug,tab_atelier::tracking=debug cargo run
```

### Power monitoring

On Intel systems with readable RAPL counters, each tab shows its estimated power usage (watts) in the right-click context menu. The estimate uses the same technique as [wattaouille](https://github.com/wdes/wattaouille): `per-tab watts = package watts × (tab CPU jiffies / total system jiffies)`. See wattaouille's README for how to make RAPL readable (`chmod` or udev rule). When RAPL is not available, only CPU percentage is shown.

### HTTP API

Tab Atelier exposes tab state on `http://<local-ip>:7890` as JSON. Access requires a bearer token, shown via a QR code in the right-click menu ("Remote control"). The response includes tab names, working directories, active tab index, and per-tab power stats. Tabs can be closed remotely via `DELETE /tabs/{index}`.

### Wakatime

Wakatime integration is automatic if your Zed settings contain `wakatime.settings.api-key`. Heartbeats are sent with project detection (walks up to find `.git`).

### State

Tab Atelier stores its state (tab names, working directories, terminal output) in:

**File:** `$XDG_STATE_HOME/tab-atelier/tabs.json` (defaults to `~/.local/state/tab-atelier/tabs.json`)

## Building

```sh
cargo build --release
```

Requires Rust 2024 edition (rustc 1.92+).

## Testing

```sh
cargo test
cargo clippy
```

## License

MPL-2.0

Terminal rendering patterns based on [Zed](https://github.com/zed-industries/zed) (Apache-2.0 / GPL-3.0).
