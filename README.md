# Swoop

A Guake-style drop-down terminal emulator for Linux (X11), built with Rust using [alacritty_terminal](https://crates.io/crates/alacritty_terminal) and [gpui](https://crates.io/crates/gpui) (Zed's GPU-accelerated UI framework).

## Features

- Drop-down terminal toggled with global F12 hotkey
- Multiple tabs with right-click context menu
- Tab rename, copy/paste, close, close all
- Session persistence (tabs and working directories saved across restarts)
- Text selection with mouse
- Scrollback history
- Shell exit detection with close/respawn confirmation
- Per-tab power usage display (Intel RAPL, same technique as [wattaouille](https://github.com/wdes/wattaouille))
- HTTP API for external integrations (tab list on `127.0.0.1:7890`)
- Wakatime time tracking (reads API key from Zed settings)

## Settings

Swoop reads font configuration from your Zed editor settings file:

**File:** `$XDG_CONFIG_HOME/zed/settings.json` (defaults to `~/.config/zed/settings.json`)

| Setting              | Description                  | Default      |
|----------------------|------------------------------|--------------|
| `ui_font_family`     | Terminal font family         | `monospace`  |
| `ui_font_weight`     | Font weight (100-900)        | `400`        |
| `ui_font_size`       | Font size in pixels          | `16`         |
| `buffer_font_size`   | Fallback if no ui_font_size  | `16`         |
| `scroll_sensitivity` | Scroll speed multiplier      | `1.0`        |

### Power monitoring

On Intel systems with readable RAPL counters, each tab shows its estimated power usage (watts) in the tab bar. The estimate uses the same technique as [wattaouille](https://github.com/wdes/wattaouille): `per-tab watts = package watts × (tab CPU jiffies / total system jiffies)`. See wattaouille's README for how to make RAPL readable (`chmod` or udev rule). When RAPL is not available, the wattage display is hidden.

### HTTP API

Swoop exposes tab state on `http://127.0.0.1:7890` as JSON for external tools (e.g. an Android companion app). The response includes tab names, working directories, active tab index, and per-tab wattage.

### Wakatime

Wakatime integration is automatic if your Zed settings contain `wakatime.settings.api-key`. Heartbeats are sent with project detection (walks up to find `.git`).

Swoop stores its own state (tab names, working directories) in:

**File:** `$XDG_STATE_HOME/swoop/tabs.json` (defaults to `~/.local/state/swoop/tabs.json`)

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
