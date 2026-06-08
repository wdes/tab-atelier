# Agents

Read CLAUDE.md first if it exists.

## Project

Tab Atelier is a Guake-style drop-down terminal emulator for Linux (X11), built with `alacritty_terminal` 0.26 and `gpui` 0.2 in Rust.

## Architecture

- `src/lib.rs` — shared types (TabState, Preferences, FontConfig), state persistence, URL detection
- `src/main.rs` — application entry, tab management (AppState struct), UI rendering via gpui
- `src/terminal.rs` — terminal emulator view (TerminalView), PTY management, grid rendering
- `src/terminal_utils.rs` — color conversion, keystroke mapping, ANSI color tables
- `src/locale.rs` — i18n with English/French translations
- `src/api.rs` — HTTP API server for remote control
- `src/power.rs` — per-tab power/energy monitoring via wattaouille
- `src/screenshot.rs` — X11 screenshot capture to BMP
- `src/schedule.rs` — per-tab off-hours auto-lock (OSM `opening_hours` + IANA tz). See `docs/schedule.md`.
- `src/tracking.rs` — Wakatime integration
- `src/platform/linux.rs` — Linux-specific platform code (XDG dirs, X11 hotkeys, process info)

## Constraints

- No in-app keyboard shortcuts. Only global hotkeys (F12). Mouse-driven UI.
- Never launch or test the app — the user runs it themselves.
- Never present test plans.
- Targets Linux/X11 only. Debian 13, rustc 1.92.

## Code style

- Strict clippy: `all`, `pedantic`, `nursery` at deny level. See `Cargo.toml` `[lints.clippy]` for specific allows.
- rustfmt: edition 2024, max_width 120, use_field_init_shorthand true.
- License header: MPL-2.0 comment at top of each source file.
- Minimal comments — only when the why is non-obvious.
- No unnecessary abstractions. Fix what's asked, nothing more.

## Build dependencies

System packages needed (Ubuntu/Debian): libvulkan-dev, libwayland-dev, libxkbcommon-dev, libxkbcommon-x11-dev, libx11-dev, libxcb1-dev, libxcb-render0-dev, libxcb-shm0-dev, libxcb-xkb-dev, libfontconfig-dev, libfreetype-dev.
