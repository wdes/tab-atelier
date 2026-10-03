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
- `src/cli/team.rs` + `src/cli/delegate.rs` — Claude-to-Claude teamwork verbs (`peers`, `note`/`notes`, `handoff`, `dispatch`). See `docs/teamwork.md`.
- `src/power.rs` — per-tab power/energy monitoring via wattaouille
- `src/screenshot.rs` — X11 screenshot capture to BMP
- `src/cli/style.rs` — per-project (folder-keyed) tab colour + badge. See `docs/tab-style.md`.
- `src/schedule.rs` — per-tab off-hours auto-lock (OSM `opening_hours` + IANA tz). See `docs/schedule.md`.
- `src/tracking.rs` — Wakatime integration
- `src/platform/linux.rs` — Linux-specific platform code (XDG dirs, X11 hotkeys, process info)

## Mobile app (the `android-app` branch)

The Android client is no longer in this repository. It is a **fork of
ConnectBot** now, living on the `android-app` branch: upstream keeps its own
history there as a submodule and our changes to it are an overlay, and that
branch's CI builds the APK and publishes it to `/android/` on the site. Nothing
in this tree builds or ships the app any more, and `/android/` is maintained
solely from that branch.

Its README documents the layout, the build, and the signing identity
(`fr.wdes.tab_atelier` plus its certificate, neither of which may ever change —
Google's ownership registration and the upgrade path of the installed app both
key on them).

Two facts from the retired Slint client are worth carrying over when the tab
list is implemented there: the app fetches `GET {url|remote_url}/tabs` with a
bearer token, and the headless origin's self-signed / Cloudflare-Origin
certificate is not trusted by Android — a WebView can wave it through, but an
HTTP client with default TLS validation rejects it and reports the host offline.

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

A **C compiler** (`build-essential` / `gcc`, or `clang`) — required by
`libmimalloc-sys`, which compiles the mimalloc C source via the `cc` crate.
mimalloc is our global allocator on both binaries (replaces glibc malloc; see
`src/main.rs` / `src/bin/tab-atelier-headless.rs`). It's **statically linked**,
so this is a build-time requirement only — no new runtime/`.deb` dependency.

System packages needed for the GUI (Ubuntu/Debian): libvulkan-dev, libwayland-dev, libxkbcommon-dev, libxkbcommon-x11-dev, libx11-dev, libxcb1-dev, libxcb-render0-dev, libxcb-shm0-dev, libxcb-xkb-dev, libfontconfig-dev, libfreetype-dev.

Without those `-dev` packages the GUI edition fails at **link** time
(`mold: fatal: library not found: xcb`) — the runtime `libxcb.so.1` is there,
the bare `libxcb.so` symlink a linker wants is not. That failure is easy to
mistake for "the GUI can't be tested here" and skip to `--lib` on the headless
config, which is how GUI-only test failures reach CI. If installing them isn't
an option, symlink the runtime libs into a directory and point the linker at
it — enough to build and run the whole suite:

```
for l in xcb xkbcommon xkbcommon-x11 z stdc++ freetype fontconfig; do
  ln -sf "$(ls /usr/lib/x86_64-linux-gnu/lib$l.so.[0-9] | head -1)" /tmp/ta-links/lib$l.so
done
RUSTFLAGS="-L /tmp/ta-links" cargo test            # gui default
RUSTFLAGS="-L /tmp/ta-links" cargo test --no-default-features --features headless
```
