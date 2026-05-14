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

## Settings

Swoop reads font configuration from your Zed editor settings file:

**File:** `$XDG_CONFIG_HOME/zed/settings.json` (defaults to `~/.config/zed/settings.json`)

| Setting           | Description              | Default      |
|-------------------|--------------------------|--------------|
| `ui_font_family`  | Terminal font family     | `monospace`  |
| `ui_font_weight`  | Font weight (100-900)    | `400`        |
| `ui_font_size`    | Font size in pixels      | `16`         |
| `buffer_font_size`| Fallback if no ui_font_size | `16`      |

Swoop stores its own state (tab names, working directories) in:

**File:** `$XDG_STATE_HOME/swoop/tabs.json` (defaults to `~/.local/state/swoop/tabs.json`)

## Building

```sh
cargo build --release
```

Requires Rust 2024 edition (rustc 1.92+).

## License

MPL-2.0

Terminal rendering patterns based on [Zed](https://github.com/zed-industries/zed) (Apache-2.0 / GPL-3.0).
