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
- `src/schedule.rs` — per-tab off-hours auto-lock (OSM `opening_hours` + IANA tz). See `docs/schedule.md`.
- `src/tracking.rs` — Wakatime integration
- `src/platform/linux.rs` — Linux-specific platform code (XDG dirs, X11 hotkeys, process info)

## Mobile app (`android/ta-remote`)

The Android remote client lives IN this repo — a Slint (Rust) app, not a
separate project:
- `src/android_app.rs` — native glue + the reachability poller and API calls.
- `src/onboard.rs` — parses the `taremote://onboard?url&tls_url&token` deep link
  (the QR from the desktop share modal — note it carries **LAN** addresses only;
  the public host is the app's separately-set `remote_url`).
- `ui/*.slint` — the UI; `java/fr/wdes/tab_atelier/WebViewHost.java` — the
  fullscreen WebView hosting the `/tabs/<id>/view` share-viewer.
- Build: `cargo-apk2` (config under `[package.metadata.android]` in its
  `Cargo.toml`; `aarch64-linux-android`, minSdk 23, pkg `fr.wdes.tab_atelier`).
  Check/build from `android/ta-remote` with `ANDROID_HOME` + `ANDROID_NDK_ROOT`
  set to an SDK carrying an NDK (25/26): `cargo apk2 check` (compile-check) /
  `cargo apk2 build` (APK). A plain `cargo check --target aarch64-linux-android`
  fails on the Slint android-activity build-script — go through `cargo apk2`.
  Host-only pure logic still runs with `cargo test --lib` there (the `onboard`
  module is unconditional; `android_app` is `cfg(android)`).

Reachability: the app polls `GET {url|remote_url}/tabs` (Bearer token) with
`ureq` → `Lan` / `Remote` / `Forbidden` (401/403) / **`Offline`** (anything
else). A 200 whose body doesn't deserialize into the app's `ApiResponse`, OR any
non-401/403 error, reads as Offline — so a `/tabs` JSON-shape change can silently
make the app report the host offline.

**TLS gotcha (a recurring "host offline" cause):** the headless origin usually
serves a self-signed / Cloudflare-Origin cert Android doesn't trust. The WebView
waves it through (`handler.proceed()` in WebViewHost.java), but the `ureq`
reachability agent (`android_app.rs`, ~`AgentBuilder::new()`) uses **default TLS
validation** — so it rejects that cert and shows Offline on remote HTTPS even
though the browser/WebView works. The bearer token is the authn material; TLS is
confidentiality only. Any reachability/API agent must accept the host's cert the
same way the WebView does.

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
