# Tab Atelier

A Guake-style drop-down terminal emulator for Linux (X11), built with Rust using [alacritty_terminal](https://crates.io/crates/alacritty_terminal) and [gpui](https://crates.io/crates/gpui) (Zed's GPU-accelerated UI framework).

![Tab Atelier screenshot](.github/docs/screenshot.webp)

## Features

**Terminal**
- Drop-down terminal toggled with global **F12** hotkey
- Full terminal emulation via alacritty_terminal (colors, scrollback, bracketed paste, ...)
- GPU-accelerated rendering via gpui
- Text selection with mouse, copy/paste from context menu
- Clickable URLs and file paths detected in terminal output
- Reset input & color for misbehaving programs

**Tabs**
- Multiple tabs with drag-and-drop reordering
- Double-click to rename, right-click context menu
- **Ctrl+Shift+T** to open a new tab (inherits working directory)
- **Alt+Tab** to cycle between tabs
- Shell exit detection with close/respawn confirmation

**Session**
- Tabs, working directories, and full terminal output persisted across restarts
- Active tab selection restored on startup

**Preferences**
- Theme selection (Dark, Tomorrow Night Blue)
- Window opacity (1%-100% slider)
- Language (English, French)
- Configurable browser and code editor for opening links

**Monitoring**
- Per-tab CPU usage, power draw (watts), energy consumption (Wh), and uptime
- Low battery warning with visual indicator

**Integration**
- HTTP API with token auth and QR code for remote tab management from a phone
- Wakatime time tracking (reads API key from Zed settings)
- Screenshots (per-tab or full app) saved as BMP

## Installation

```sh
cargo build --release
# Binary at target/release/tab-atelier
```

Requires Rust 2024 edition (rustc 1.92+).

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

### Preferences

In-app preferences (theme, opacity, language, browser, code editor) are stored in:

**File:** `$XDG_STATE_HOME/tab-atelier/preferences.json` (defaults to `~/.local/state/tab-atelier/preferences.json`)

### State

Tab Atelier persists session state (tab names, working directories, terminal output, active tab) in:

**File:** `$XDG_STATE_HOME/tab-atelier/tabs.json` (defaults to `~/.local/state/tab-atelier/tabs.json`)

## Power monitoring

On Intel systems with readable RAPL counters, each tab shows its estimated power usage in the right-click context menu. The estimate uses the same technique as [wattaouille](https://github.com/wdes/wattaouille): `per-tab watts = package watts * (tab CPU jiffies / total system jiffies)`. See wattaouille's README for how to make RAPL readable (`chmod` or udev rule). When RAPL is not available, only CPU percentage is shown.

## HTTP API

Tab Atelier exposes tab state on `http://<local-ip>:7890` as JSON. Access requires a bearer token, shown via a QR code in the right-click menu ("Remote control"). The response includes tab names, working directories, active tab index, and per-tab power stats. Tabs can be closed remotely via `DELETE /tabs/{index}`.

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

## License

MPL-2.0

Terminal rendering patterns based on [Zed](https://github.com/zed-industries/zed) (Apache-2.0 / GPL-3.0).
