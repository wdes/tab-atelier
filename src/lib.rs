// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// unwrap_used + expect_used are denied crate-wide (Cargo.toml); tests may panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

pub mod agent_probe;
pub mod agent_reaper;
pub mod alloc_count;
pub(crate) mod api;
pub(crate) mod api_ws;
#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub(crate) mod box_drawing;
#[cfg(feature = "catbus")]
pub(crate) mod catbus_agent;
// Shared by both binaries now (GUI applies per-tab cgroup limits too); the
// module's own `#![cfg(target_os = "linux")]` scopes it to Linux.
#[cfg(target_os = "linux")]
pub(crate) mod cgroup;
pub mod cli;
/// The shared alacritty `EventListener` both editions attach to their `Term`.
pub(crate) mod event_proxy;
pub(crate) use event_proxy::EventProxy;
#[cfg(not(feature = "gui"))]
pub mod headless;
/// Experimental HTTP/3 + WebTransport transport (behind `http3`).
#[cfg(feature = "http3")]
pub mod http3;
pub(crate) mod locale;
#[cfg(target_os = "linux")]
pub mod log_ring;
pub mod net_meter;
#[cfg(all(target_os = "linux", not(feature = "gui")))]
pub mod net_nft;
pub mod net_policy;
#[cfg(all(target_os = "linux", not(feature = "gui")))]
pub mod net_resolver;
#[cfg(feature = "pets")]
pub mod pet;
pub(crate) mod platform;
#[cfg(feature = "energy")]
pub(crate) mod power;
pub(crate) mod pty_ring;
pub mod relay;
pub mod remote;
pub mod schedule;
#[cfg(feature = "gui")]
pub(crate) mod screenshot;
pub mod ssh_agent;
pub(crate) mod term_export;
#[cfg(feature = "gui")]
pub(crate) mod terminal;
#[cfg(feature = "gui")]
pub(crate) mod terminal_utils;
pub(crate) mod theme;
pub(crate) mod tracking;
pub mod transcript_compact;
#[cfg(all(windows, not(feature = "gui")))]
pub mod win_service;

pub const APP_DIR: &str = "tab-atelier";

/// `log` target for the keystroke/input-latency trace.
///
/// The trace logs `key` / `key_char` per key event — the data the IME
/// bug needs. Single source of truth: the `trace!` call sites and the
/// `tab-atelier log input` preset both reference this, so a rename can't
/// leave them out of sync.
pub const INPUT_TRACE_TARGET: &str = "tab_atelier::input_lag";

/// Set by the SIGINT/SIGTERM handler. The persist tick checks it and runs
/// `close_all_tabs` (which does an unconditional flush of every tab's
/// output / uptime / energy file) before letting gpui shut down.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set to true when `--read-only` was passed.
///
/// In read-only mode the app does not acquire the single-instance lock,
/// never writes any persisted state, and disables the preferences "Save"
/// button. Useful for inspecting an existing workspace alongside a normal
/// instance.
pub static READ_ONLY: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn read_only() -> bool {
    READ_ONLY.load(Ordering::SeqCst)
}

/// When set, every tab's PTY is spawned in a *cleared* environment.
///
/// PHP-FPM `clear_env = yes` style: the shell carries only the curated
/// minimal allowlist — see [`minimal_pty_env`]. Off by default, because
/// clearing drops `DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` /
/// `SSH_AUTH_SOCK` / … which GUI apps and ssh-agent need, so it's opt-in
/// via the `clear_env` preference. Set once at startup, like [`READ_ONLY`].
pub static CLEAR_ENV: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn clear_env() -> bool {
    CLEAR_ENV.load(Ordering::SeqCst)
}

/// "Claude-only" forced mode: every new tab launches `claude`.
///
/// New tabs run `claude` (auto-accept-edits) instead of a shell. Unlike
/// [`READ_ONLY`]/[`CLEAR_ENV`] this is **runtime-mutable** — the right-click
/// "New bash tab" item cancels it live — and seeded from either the
/// `--claude-only` flag or the `claude_only` preference. The GUI mirrors it
/// onto its own struct field for the menu.
pub static CLAUDE_ONLY: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn claude_only() -> bool {
    CLAUDE_ONLY.load(Ordering::SeqCst)
}

pub fn set_claude_only(on: bool) {
    CLAUDE_ONLY.store(on, Ordering::SeqCst);
}

/// Relay mode: forward this instance's Claude tabs' Anthropic API calls
/// through a configured remote tab-atelier (see `src/relay.rs`).
///
/// Runtime-mutable like [`CLAUDE_ONLY`]; seeded from the `--relay` flag or the
/// `relay_mode` preference. When on, [`tab_env_extras`] injects
/// `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` so every claude tab talks to the
/// local relay listener instead of `api.anthropic.com` directly.
pub static RELAY_MODE: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn relay_mode() -> bool {
    RELAY_MODE.load(Ordering::SeqCst)
}

pub fn set_relay_mode(on: bool) {
    RELAY_MODE.store(on, Ordering::SeqCst);
}

/// Egress role: this instance terminates `/relay/anthropic/*` and forwards to
/// `api.anthropic.com` using its own Claude login (set on the REMOTE).
pub static RELAY_EGRESS: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn relay_egress() -> bool {
    RELAY_EGRESS.load(Ordering::SeqCst)
}

pub fn set_relay_egress(on: bool) {
    RELAY_EGRESS.store(on, Ordering::SeqCst);
}

/// Resolved forward target for the LOCAL relay hop: the remote tab-atelier's
/// URL + credentials (from the `relay_endpoint_id` preference). `None` when
/// unconfigured or when this instance is the egress.
#[derive(Clone)]
pub struct RelayTarget {
    pub url: String,
    pub token: String,
    pub cf_access_client_id: String,
    pub cf_access_client_secret: String,
}

static RELAY_TARGET: std::sync::RwLock<Option<RelayTarget>> = std::sync::RwLock::new(None);

pub fn set_relay_target(target: Option<RelayTarget>) {
    if let Ok(mut g) = RELAY_TARGET.write() {
        *g = target;
    }
}

#[must_use]
pub fn relay_target() -> Option<RelayTarget> {
    RELAY_TARGET.read().ok().and_then(|g| g.clone())
}

/// Apply a `relay via` / `relay egress` change.
///
/// Resolves the endpoint (by id or label, case-insensitive), persists the
/// preference (unless read-only), and re-installs the live config. Shared by
/// the GUI + headless drains.
pub fn apply_relay_config(change: &crate::api::RelayConfigChange, config_base: &std::path::Path) {
    let mut prefs = load_preferences(config_base);
    if let Some(ep) = &change.endpoint {
        prefs.relay_endpoint_id = if ep.is_empty() {
            None
        } else {
            // Prefer resolving a friendly label to its stable id; fall back to
            // treating the value as an id so an unknown value still records.
            prefs
                .remote_endpoints
                .iter()
                .find(|e| e.id == *ep || e.label.eq_ignore_ascii_case(ep))
                .map(|e| e.id.clone())
                .or_else(|| Some(ep.clone()))
        };
    }
    if let Some(eg) = change.egress {
        prefs.relay_egress = eg;
    }
    if !read_only() {
        save_preferences(config_base, &prefs);
    }
    install_relay_config(&prefs);
}

/// Resolve + install the relay egress flag and forward target from a loaded
/// The credential to present to a peer's relay route.
///
/// One endpoint entry serves two different consumers: the sidecar needs the
/// peer's MASTER token (it lists tabs, types input, moves files), while the
/// relay must present the peer's RELAY token, which can do nothing but proxy.
/// So `relay_token` wins when set, and `token` remains the fallback for an
/// entry that predates the split or points at a relay-only peer.
#[must_use]
pub fn relay_credential(endpoint: &RemoteEndpoint) -> &str {
    if endpoint.relay_token.is_empty() {
        &endpoint.token
    } else {
        &endpoint.relay_token
    }
}

/// `Preferences`. Called at startup (both editions) and after a relay toggle.
pub fn install_relay_config(prefs: &Preferences) {
    set_relay_egress(prefs.relay_egress);
    let target = prefs.relay_endpoint_id.as_deref().and_then(|id| {
        prefs.remote_endpoints.iter().find(|e| e.id == id).map(|e| RelayTarget {
            url: e.url.trim_end_matches('/').to_string(),
            token: relay_credential(e).to_string(),
            cf_access_client_id: e.cf_access_client_id.clone(),
            cf_access_client_secret: e.cf_access_client_secret.clone(),
        })
    });
    set_relay_target(target);
}

/// Global user env vars injected into EVERY tab's PTY (the CLI
/// `env set --global KEY=VAL`).
///
/// Runtime-mutable — unlike [`clear_env_user_vars`] (startup-only) — so a live
/// `env set` takes effect on the next tab spawn. Seeded from the `tab_env`
/// preference at startup and replaced wholesale by the env drain. Lowest
/// priority in the layered PTY env: the functional `_TAB_ID`/`TAB_ATELIER_*`
/// vars (and the relay injection) win over it.
static TAB_ENV_GLOBAL: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

pub fn set_tab_env_global(vars: std::collections::BTreeMap<String, String>) {
    if let Ok(mut g) = TAB_ENV_GLOBAL.write() {
        *g = vars;
    }
}

#[must_use]
pub fn tab_env_global() -> std::collections::BTreeMap<String, String> {
    TAB_ENV_GLOBAL.read().map(|g| g.clone()).unwrap_or_default()
}

/// Per-folder tab styles from the `folder_styles` preference, resolved on
/// every tab spawn / snapshot. Set once at startup, like the other
/// preference-backed globals; editing the preference takes effect on the next
/// daemon start (same contract as `bg-color --global`).
static FOLDER_STYLES: std::sync::RwLock<std::collections::BTreeMap<String, FolderStyle>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Replace the process's per-folder styles. Called at startup and again by
/// [`refresh_folder_styles`] whenever the preference file changes.
pub fn set_folder_styles(styles: std::collections::BTreeMap<String, FolderStyle>) {
    if let Ok(mut g) = FOLDER_STYLES.write() {
        *g = styles;
    }
}

/// The folder rule that applies to `cwd`, resolved and owned.
///
/// Owned rather than borrowed so the caller doesn't hold the lock: callers
/// resolve once per tick and cache the result on the tab, so painting a frame
/// never touches this.
#[must_use]
pub fn folder_style_of(cwd: Option<&str>) -> FolderStyle {
    FOLDER_STYLES
        .read()
        .ok()
        .and_then(|g| folder_style_for(&g, cwd).cloned())
        .unwrap_or_default()
}

/// mtime of the preference file at the last [`refresh_folder_styles`].
static FOLDER_STYLES_MTIME: std::sync::Mutex<Option<std::time::SystemTime>> = std::sync::Mutex::new(None);

/// Re-read `folder_styles` when preferences.json changed on disk.
///
/// Called from each edition's tick, so `style --folder` lands on a running
/// desktop instead of waiting for a restart — which, with a few dozen tabs
/// open, is not a thing anyone wants to do to try a colour. Only the mtime is
/// stat'd on the common path; the file is parsed solely when it moved.
pub fn refresh_folder_styles() {
    let Ok(mtime) = std::fs::metadata(editable_preferences_path()).and_then(|m| m.modified()) else {
        return;
    };
    let mut last = FOLDER_STYLES_MTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *last == Some(mtime) {
        return;
    }
    *last = Some(mtime);
    // Released before the parse + the styles write lock — nothing else needs
    // to wait on the stamp while we read the file.
    drop(last);
    set_folder_styles(load_preferences(&platform::config_dir()).folder_styles);
}

/// User-defined `key=value` pairs from the `clear_env_vars` preference,
/// layered into every cleared-env tab (see [`minimal_pty_env`]). Set
/// once at startup; reads after that are lock-free. Empty until set.
static CLEAR_ENV_USER_VARS: OnceLock<std::collections::BTreeMap<String, String>> = OnceLock::new();

/// Install the user's `clear_env_vars` for this process. No-op if called
/// twice (first set wins) — startup is the only caller.
pub fn set_clear_env_user_vars(vars: std::collections::BTreeMap<String, String>) {
    let _ = CLEAR_ENV_USER_VARS.set(vars);
}

/// The user's `clear_env_vars`, or an empty map if none were set.
#[must_use]
pub fn clear_env_user_vars() -> &'static std::collections::BTreeMap<String, String> {
    static EMPTY: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    CLEAR_ENV_USER_VARS.get().unwrap_or(&EMPTY)
}

/// Kept alive for the lifetime of the process so the file lock isn't
/// released until the process exits.
static INSTANCE_LOCK: OnceLock<std::fs::File> = OnceLock::new();

/// Build the per-tab env map for `_TAB_ID` / `TAB_ATELIER_API_URL` /
/// `TAB_ATELIER_API_TOKEN`.
///
/// Both binaries inject these at PTY spawn time so any tool running
/// inside the tab can locate the local API without manual config
/// (the `tab-atelier set-status` / `tabs` subcommands both rely on
/// them).
#[must_use]
pub fn tab_env_extras(
    tab_id: &str,
    api_url: &str,
    api_token: &str,
    per_tab: &std::collections::BTreeMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    // Layered lowest→highest: global user env (`env set --global`), then per-tab
    // env (`env set --tab`), then the functional vars, then relay. The user env
    // deliberately can't shadow the functional/relay vars.
    for (k, v) in tab_env_global() {
        m.insert(k, v);
    }
    for (k, v) in per_tab {
        m.insert(k.clone(), v.clone());
    }
    m.insert("_TAB_ID".into(), tab_id.to_string());
    m.insert("TAB_ATELIER_API_URL".into(), api_url.to_string());
    m.insert("TAB_ATELIER_API_TOKEN".into(), api_token.to_string());
    // Relay mode: point every claude tab at the local relay listener. `api_url`
    // is the local API base (`http://127.0.0.1:<port>`); the relay route lives
    // under `/relay/anthropic`.
    //
    // The stand-in `ANTHROPIC_API_KEY` is the RELAY token, not the master one.
    // An API key is a value tools copy around — into debug output, crash
    // reports, shared transcripts — and the master token administers every tab
    // in the instance. The relay token only authenticates to the relay route.
    if relay_mode() {
        m.insert(
            "ANTHROPIC_BASE_URL".into(),
            format!("{}/relay/anthropic", api_url.trim_end_matches('/')),
        );
        m.insert("ANTHROPIC_API_KEY".into(), relay_token());
    }
    m
}

/// Env vars forced into **every** tab's PTY to disable Claude Code's
/// telemetry, feedback surveys, and other nonessential traffic.
///
/// Set on all tabs unconditionally so no agent session running inside a
/// tab phones home or prompts for the feedback survey.
///
/// - `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` is the strongest
///   switch: it disables the rating/feedback survey, the transcript-
///   share follow-up, and all other Anthropic-bound feedback traffic.
/// - `DISABLE_TELEMETRY=1` and `DO_NOT_TRACK=1` are the widely-honoured
///   opt-out signals (they also independently disable the survey).
/// - `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY=1` is the explicit survey
///   kill-switch, set as belt-and-suspenders.
///
/// We deliberately do NOT set `CLAUDE_CODE_ENABLE_FEEDBACK_SURVEY_FOR_OTEL`
/// (which would opt the survey back in for an org's OTEL collector).
pub const TELEMETRY_DISABLE_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DO_NOT_TRACK", "1"),
    ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
];

/// Insert [`TELEMETRY_DISABLE_ENV`] into a PTY env map. Called by both
/// the GUI and headless spawn paths so the opt-out applies to every
/// tab on every spawn (initial spawn and respawn).
pub fn apply_telemetry_disable_env<S: std::hash::BuildHasher>(env: &mut std::collections::HashMap<String, String, S>) {
    for (k, v) in TELEMETRY_DISABLE_ENV {
        env.insert((*k).to_string(), (*v).to_string());
    }
}

/// Per-tab environment extras for the NORMAL (non-cleared) spawn path.
///
/// The colour vars from the tab's own flag, plus the telemetry opt-out;
/// layered on top of the inherited parent environment by the caller. Shared by
/// both editions (the GUI's `terminal.rs` and the headless daemon) so the two
/// spawn paths can't drift; the cleared-env path uses [`minimal_pty_env`].
#[must_use]
pub fn pty_env(colors_enabled: bool) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    if colors_enabled {
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
    } else {
        env.insert("TERM".into(), "dumb".into());
    }
    // Force the telemetry / feedback-survey opt-out onto every tab.
    apply_telemetry_disable_env(&mut env);
    env
}

/// Parent-environment variables carried over into a cleared-env tab.
///
/// Everything NOT on this list is dropped (the `clear_env` opt-in,
/// modelled on PHP-FPM's `clear_env = yes`). Categories:
///
/// - **Path:** `PATH` — without it the shell can't find any command.
/// - **Identity / username:** `HOME`, `USER`, `LOGNAME`.
/// - **Shell:** `SHELL`.
/// - **Locale (UTF-8 rendering / sorting):** `LANG`, `LANGUAGE`,
///   `LC_ALL`, `LC_CTYPE`.
/// - **Timezone:** `TZ`.
///
/// Colours (`TERM` / `COLORTERM`) are NOT sourced from the parent —
/// they're set from the tab's own colours flag in [`minimal_pty_env`],
/// same as the normal (non-cleared) spawn path. Sensitive / session
/// vars (`DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`,
/// `XAUTHORITY`, `AWS_*`, `*_TOKEN`, …) are deliberately absent — that
/// omission is the whole point of the feature.
pub const CLEAR_ENV_KEEP: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "TZ",
];

/// Fallback `PATH` when the parent process has none — an empty `PATH`
/// in a cleared environment leaves the shell unable to resolve even
/// `ls`, so seed a conventional system default.
pub const CLEAR_ENV_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Build the *complete* minimal environment for a cleared-env tab.
///
/// Layers, lowest priority first: the [`CLEAR_ENV_KEEP`] allowlist
/// sourced from the current process, colour vars (from `colors_enabled`),
/// then the user's settings-file `clear_env_vars` (which win over those
/// basics), then the telemetry opt-out, then `extra_env` (the per-tab
/// API vars). This is the only environment the shell will see —
/// nothing is inherited.
#[must_use]
pub fn minimal_pty_env<S: std::hash::BuildHasher>(
    colors_enabled: bool,
    user_env: &std::collections::BTreeMap<String, String>,
    extra_env: &std::collections::HashMap<String, String, S>,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    // 1. Kept system basics from the parent process.
    for &key in CLEAR_ENV_KEEP {
        if let Ok(val) = std::env::var(key)
            && !val.is_empty()
        {
            env.insert(key.to_string(), val);
        }
    }
    env.entry("PATH".to_string())
        .or_insert_with(|| CLEAR_ENV_DEFAULT_PATH.to_string());
    // 2. Colours: identical policy to the inheriting `pty_env` path.
    if colors_enabled {
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
    } else {
        env.insert("TERM".to_string(), "dumb".to_string());
    }
    // 3. Telemetry opt-out (tab-atelier privacy default).
    apply_telemetry_disable_env(&mut env);
    // 4. User-defined vars from the settings file — these WIN over the
    //    kept basics, colours and telemetry above ("if user has the same
    //    key, user wins").
    for (k, v) in user_env {
        env.insert(k.clone(), v.clone());
    }
    // 5. Per-tab API vars (`_TAB_ID`, `TAB_ATELIER_API_*`). Applied last
    //    so the in-tab tooling keeps working — these are functional, not
    //    a user preference, and aren't meant to be overridden.
    for (k, v) in extra_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Absolute path to `env(1)` used to launch a cleared-env shell. Fixed
/// absolute path (not PATH-resolved) so spawning doesn't depend on the
/// parent `PATH` and can't be shadowed.
pub const ENV_BIN: &str = "/usr/bin/env";

/// Build the `(program, args)` to spawn `shell` in a *cleared*
/// environment containing only `env`.
///
/// alacritty's `tty` always inherits the parent environment and only
/// overlays `Options.env` (it exposes no env-clear), so the portable
/// way to truly start from empty is to exec `env -i K=V … <shell>`:
/// `env -i` ignores its own inherited environment and runs the shell
/// with exactly the listed variables. The caller sets this as
/// `Options.shell` and leaves `Options.env` empty.
///
/// `login` appends `-l` so the shell sources the profile files (the GUI
/// wants this); the headless daemon passes `false` because a login
/// shell sources `/etc/profile` / `~/.profile` which fail noisily for
/// the service account that has no profile under `ProtectHome=true`.
#[must_use]
pub fn clear_env_shell_command<S: std::hash::BuildHasher>(
    shell: &str,
    login: bool,
    env: &std::collections::HashMap<String, String, S>,
) -> (String, Vec<String>) {
    let mut args: Vec<String> = Vec::with_capacity(env.len() + 3);
    args.push("-i".to_string());
    for (k, v) in env {
        args.push(format!("{k}={v}"));
    }
    args.push(shell.to_string());
    if login {
        args.push("-l".to_string());
    }
    (ENV_BIN.to_string(), args)
}

/// `bwrap` (bubblewrap) executable name — used to give a tab its own
/// empty network namespace so it has no internet.
const BWRAP_BIN: &str = "bwrap";

/// True when `bwrap` is on `PATH`. Net-off tabs need it; if absent, the
/// toggle is refused with a message rather than silently leaving the net
/// on. Probes `PATH` entries without executing anything.
#[must_use]
pub fn bwrap_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(BWRAP_BIN).is_file()))
}

/// Wrap a shell command so the tab has **no internet**: run it inside a
/// bubblewrap sandbox with an isolated network namespace (loopback only).
///
/// - `--dev-bind / /` keeps the whole host filesystem visible (tools,
///   profiles, the user's `$HOME` all work as normal),
/// - `--proc /proc` mounts a fresh `/proc` so it reflects the empty netns,
/// - `--unshare-net` is the actual airgap (only `lo`, no route/DNS),
/// - `--die-with-parent` ties the sandbox's life to tab-atelier.
///
/// bubblewrap runs unprivileged via user namespaces (no `CAP_NET_ADMIN`),
/// so this works in both the desktop GUI and the headless service.
/// Returns the `(program, args)` to hand to the PTY.
#[must_use]
pub fn no_internet_command(prog: &str, args: &[String]) -> (String, Vec<String>) {
    let mut out: Vec<String> = [
        "--dev-bind",
        "/",
        "/",
        "--proc",
        "/proc",
        "--unshare-net",
        "--die-with-parent",
        "--",
        prog,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    out.extend(args.iter().cloned());
    (BWRAP_BIN.to_string(), out)
}

/// `setpriv` (util-linux) executable name — used to strip Linux
/// capabilities from a tab's shell subtree.
const SETPRIV_BIN: &str = "setpriv";

/// True when `setpriv` is on `PATH` (util-linux; essentially always on
/// Debian). Probed without executing.
#[must_use]
pub fn setpriv_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(SETPRIV_BIN).is_file()))
}

/// Whether this process holds any **ambient** capabilities.
///
/// Ambient caps are the ones a child inherits across `exec`. Tabs only need
/// stripping (via [`drop_caps_command`]) when there's something to strip: if
/// the daemon has no ambient caps (the default — `AmbientCapabilities=`
/// empty), wrapping every shell in `setpriv` is pointless AND dangerous,
/// because `setpriv` calls `capset`, which a restrictive `SystemCallFilter`
/// may block — and then the shell never execs (a blank tab). Reads
/// `/proc/self/status`; `false` on non-Linux or any read/parse failure.
#[must_use]
pub fn has_ambient_caps() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status").is_ok_and(|status| cap_amb_nonzero(&status))
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Whether the `CapAmb:` line of a `/proc/<pid>/status` blob is non-zero.
/// Pure so the gating logic is unit-testable. `false` when absent/unparseable.
#[cfg(target_os = "linux")]
#[must_use]
fn cap_amb_nonzero(status: &str) -> bool {
    status.lines().any(|line| {
        line.strip_prefix("CapAmb:")
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            .is_some_and(|v| v != 0)
    })
}

/// Wrap a shell command so the tab's process subtree holds **no Linux
/// capabilities** and can never regain any.
///
/// The headless service is granted `CAP_NET_ADMIN` (to program the per-tab
/// nftables egress allowlist), which systemd places in the daemon's
/// *ambient* set — and ambient caps are inherited across `exec` into every
/// child. Without stripping them, an agent inside a tab could `nft flush`
/// its own allowlist and walk straight out. So we drop them:
///
/// - `--ambient-caps=-all` clears the ambient set the child would inherit
///   (this is the one that actually carries `CAP_NET_ADMIN` to the tab),
/// - `--inh-caps=-all` clears inheritable so nothing re-populates ambient,
/// - `--no-new-privs` blocks regaining privileges via setuid/`execve`.
///
/// Bounding-set drop is intentionally omitted: it needs `CAP_SETPCAP`,
/// while the three above work for an ordinary (even capability-holding)
/// user, and clearing ambient is sufficient to deny the cap. Returns the
/// `(program, args)` to hand to the PTY.
#[must_use]
pub fn drop_caps_command(prog: &str, args: &[String]) -> (String, Vec<String>) {
    let mut out: Vec<String> = ["--ambient-caps=-all", "--inh-caps=-all", "--no-new-privs", "--", prog]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.extend(args.iter().cloned());
    (SETPRIV_BIN.to_string(), out)
}

/// The login shell to run inside a cleared-env tab.
///
/// Read from `$SHELL` (the only place the parent's choice survives once
/// we clear), falling back to `/bin/bash`. Returned as an absolute path
/// candidate so `env -i` can exec it without a `PATH` lookup.
#[must_use]
pub fn clear_env_shell_path() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string())
}

/// Best-effort SIGTERM→SIGKILL of a tab's process **group**.
///
/// The PTY child is a session/group leader (alacritty's `tty::new` calls
/// `setsid`), so `pid == pgid`; killing the group `-pid` takes down the shell
/// and its foreground job (e.g. `claude`), which a bare PTY-close SIGHUP can
/// survive and orphan. Used on the desktop, which has no cgroups — the headless
/// daemon prefers [`crate::cgroup::kill_tab`] (kills the *whole* subtree).
///
/// `unsafe`-free: shells to `kill(1)` with the `-s SIG -- -PGID` form
/// (util-linux needs the `--` before the negative pgid).
#[cfg(unix)]
pub fn kill_tab_pgroup(pid: u32) {
    if pid <= 1 {
        return; // never signal pid 0 (our group) or init
    }
    let target = format!("-{pid}");
    for sig in ["TERM", "KILL"] {
        let _ = std::process::Command::new("kill")
            .args(["-s", sig, "--", &target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Build the value handed to in-tab tools as `TAB_ATELIER_API_URL`.
///
/// The stored `api_addr` is a bind spec (`0.0.0.0:7890`, `:7890`,
/// `127.0.0.1:9000`); we always rewrite the host to `127.0.0.1`
/// because in-tab tools live on the same machine.
#[must_use]
pub fn api_url_for_local_clients(api_addr: &str) -> String {
    let port = api_addr
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_API_PORT);
    format!("http://127.0.0.1:{port}")
}

/// Shell fragment that wipes the terminal — scrollback (`ESC[3J`), cursor home
/// (`ESC[H`), then the visible screen (`ESC[2J`) — before an agent is launched.
///
/// Agents like `claude` repaint their UI in place with cursor positioning and
/// per-line erases; they never clear the rows *below* their UI. So when one is
/// (re)launched over a grid that still holds a previous run's output — most
/// visibly the `Resume this session with: claude --resume …` line a prior
/// `claude` prints on exit — that stale tail survives at the bottom of the
/// screen (and in the byte ring the web viewer replays). Emitting a full clear
/// first gives the agent a clean grid to paint on. `printf` is a shell builtin,
/// so this needs nothing on PATH and works in the cleared-env minimal shell.
pub const AGENT_LAUNCH_CLEAR: &str = r"printf '\033[3J\033[H\033[2J'; ";

/// Command a fresh Claude-only tab runs.
///
/// `claude` started directly in `auto` permission mode — the classifier-based
/// "⏵⏵ auto mode on" mode (distinct from `acceptEdits`; see the README).
/// Used both as the exec target under cleared-env and as the string typed
/// into the shell otherwise. See [`crate::CLAUDE_ONLY`].
pub const FRESH_CLAUDE_AUTO_CMD: &str = "claude --permission-mode auto";

/// Shell args that make a fresh Claude-only tab `exec` the agent directly.
///
/// The no-session analogue of [`agent_launch_shell_suffix`] (same
/// `-i -c 'exec …'` shape, so the tab's foreground process *is* claude,
/// running [`FRESH_CLAUDE_AUTO_CMD`]). Only used under cleared-env; the
/// normal env types the command in via `pending_agent_resume`.
#[must_use]
pub fn fresh_claude_launch_suffix() -> Vec<String> {
    vec![
        "-i".to_string(),
        "-c".to_string(),
        format!("{AGENT_LAUNCH_CLEAR}exec {FRESH_CLAUDE_AUTO_CMD}"),
    ]
}

/// Our own CLI binary name — the two editions ship different ones (the debs
/// conflict, so each carries only its own on PATH).
#[must_use]
pub const fn cli_binary_name() -> &'static str {
    #[cfg(feature = "gui")]
    {
        "tab-atelier"
    }
    #[cfg(not(feature = "gui"))]
    {
        "tab-atelier-headless"
    }
}

/// Is `kind` a **session-less daemon** tab rather than a resumable agent?
///
/// That's one of our own CLI subcommands run as a tab — `⛑ brain`, and
/// whatever watcher a harness registers with `set-status --kind <verb>`.
///
/// Charset-gated to a plain lowercase verb so [`build_agent_resume_command`]
/// can only ever reconstruct OUR binary running one of its own subcommands —
/// no spaces, no shell metacharacters, and an unknown verb simply exits 2.
#[must_use]
pub fn is_daemon_kind(kind: &str) -> bool {
    !matches!(kind, "catbus" | "claude")
        && (2..=24).contains(&kind.len())
        && kind.starts_with(|c: char| c.is_ascii_lowercase())
        && kind.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// The restore command for a **session-less daemon tab**: our own binary
/// running the subcommand named by `kind`.
///
/// Only ever called for a tab that declared itself one (`set-status --kind
/// <verb> --daemon`, persisted as [`TabState::agent_daemon`]) — an
/// unrecognised `agent_kind` on its own still restores to a plain shell.
/// The charset gate means the reconstructed command is always OUR binary plus
/// one subcommand: no spaces, no shell metacharacters, and an unknown verb
/// simply exits 2.
#[must_use]
pub fn daemon_relaunch_command(kind: &str) -> Option<String> {
    is_daemon_kind(kind).then(|| format!("{} {kind}", cli_binary_name()))
}

/// Translate a persisted (`agent_kind`, `session_id`, `plan_mode`) into
/// the shell command to type for auto-resume. Returns None when the
/// `agent_kind` isn't one we know how to drive.
#[must_use]
pub fn build_agent_resume_command(kind: &str, session_id: &str, plan: Option<bool>) -> Option<String> {
    match kind {
        "catbus" => {
            let flag = if plan == Some(true) { " --plan" } else { "" };
            Some(format!("catbus-agent --resume {session_id}{flag}"))
        }
        "claude" => Some(format!("claude --resume {session_id}")),
        // The ⛑ brain watchdog has no session to resume — it's a standalone
        // tool that re-attaches to every OTHER tab over the local API, so
        // restore just relaunches it. `session_id` is unused. Brain predates
        // the `agent_daemon` flag, hence the name here; any other daemon goes
        // through [`daemon_relaunch_command`].
        "brain" => daemon_relaunch_command("brain"),
        _ => None,
    }
}

/// How a (re)spawn brings an agent tab back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRelaunch {
    /// Cleared-env mode can drive the shell command line, so the tab's process
    /// IS the agent (`… -i -c 'exec claude --resume <id>'`).
    Exec,
    /// Otherwise the shell is forked normally and the resume command is typed
    /// in once it prompts (`pending_agent_resume`).
    Typed,
    /// Not an agent tab, or read-only: leave the plain shell alone.
    None,
}

/// Decide how a respawn relaunches an agent tab.
///
/// Exactly one of exec/typed, never both (that double-launches the session) and
/// never neither — "neither" is what silently turned a colours or net toggle on
/// a claude tab into a bare shell. Read-only never resumes: `claude --resume`
/// against a live session rotates the user's session ids.
#[must_use]
pub const fn agent_relaunch_mode(has_session: bool, cleared_env: bool, read_only: bool) -> AgentRelaunch {
    if !has_session || read_only {
        AgentRelaunch::None
    } else if cleared_env {
        AgentRelaunch::Exec
    } else {
        AgentRelaunch::Typed
    }
}

/// Extra shell args that make a restored agent tab **launch the agent
/// directly** instead of dropping to a shell and typing the resume command.
///
/// Appended to the tab's shell invocation, they become e.g.
/// `bash [-l] -i -c 'exec claude --resume <id>'`:
/// - `-i` so the shell still sources the user's rc (nvm / PATH where `claude`
///   and `node` live) — same PATH the interactive tab had, so the binary
///   resolves;
/// - `exec` so the shell process is **replaced by** the agent: the tab's
///   foreground process *is* `claude`, so PTY-close / cgroup.kill / pgroup-kill
///   hit it directly (no bash middleman to orphan it), and there's no
///   type-into-shell race that can double-launch a `--resume`.
///
/// Returns None for an unknown `agent_kind` (the caller then spawns a plain
/// shell). The tab dies when the agent exits — the GUI already shows an
/// exited tab (`has_exited`) and can respawn it.
#[must_use]
pub fn agent_launch_shell_suffix(kind: &str, session_id: &str, plan: Option<bool>) -> Option<Vec<String>> {
    let cmd = build_agent_resume_command(kind, session_id, plan)?;
    // Clear the grid before `exec` so a previous run's tail doesn't linger under
    // the fresh agent UI — see [`AGENT_LAUNCH_CLEAR`].
    Some(vec![
        "-i".to_string(),
        "-c".to_string(),
        format!("{AGENT_LAUNCH_CLEAR}exec {cmd}"),
    ])
}

/// Tracer-wrapping variant of [`agent_launch_shell_suffix`].
///
/// Wraps the agent under a syscall-counting tracer (`strace -f -c`) when
/// instrumentation is on — the launch half of the resource-probe feature
/// (see [`crate::agent_probe`]). The per-session histogram lands in
/// `agent_trace_<kind>_<session>.txt` under the state dir and flushes
/// when the agent exits.
///
/// Tracing is opt-in (off by default; enable with `flags trace on` or
/// `TAB_ATELIER_AGENT_TRACE=1`). Falls back to the plain suffix when it's
/// off or no tracer is on `PATH`, so a build without `strace` still
/// launches agents normally. Call sites use this in place of the plain
/// builder; the plain one stays pure for the unit tests.
///
/// `proctitle` sets the launched process's `argv[0]` (via bash/zsh
/// `exec -a`), which the agent runtime turns into its `comm` — so
/// `top -H` / `ps` show the tab name instead of a wall of identical
/// `claude`s. Pass `None` (or a shell that lacks `exec -a`, see
/// [`shell_supports_exec_a`]) to skip it. Names the outermost program:
/// `claude` normally, or the `strace` wrapper when tracing is on.
#[must_use]
pub fn agent_launch_shell_suffix_instrumented(
    kind: &str,
    session_id: &str,
    plan: Option<bool>,
    proctitle: Option<&str>,
) -> Option<Vec<String>> {
    let cmd = build_agent_resume_command(kind, session_id, plan)?;
    let trace = agent_probe::resolve_tracer().map(|tracer| {
        let base = agent_probe::state_base();
        let log = agent_probe::trace_log_path(&base, kind, session_id);
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        (tracer, log.to_string_lossy().into_owned())
    });
    // Opt-in (`TAB_ATELIER_FRAME_TIMING`): make the agent log per-frame
    // render timings to a per-session JSONL, for idle-CPU/repaint debugging.
    let frames = agent_probe::frame_timing_enabled().then(|| {
        let base = agent_probe::state_base();
        let log = agent_probe::frame_log_path(&base, kind, session_id);
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        log.to_string_lossy().into_owned()
    });
    // Clear the grid before the agent execs so a previous run's tail (e.g. the
    // `claude --resume …` exit line) doesn't linger under the fresh UI — see
    // [`AGENT_LAUNCH_CLEAR`].
    let launch = format!(
        "{AGENT_LAUNCH_CLEAR}{}",
        wrap_exec_command(&cmd, trace.as_ref(), frames.as_deref(), proctitle)
    );
    Some(vec!["-i".to_string(), "-c".to_string(), launch])
}

/// True when `shell_path`'s `exec` builtin accepts `-a argv0`.
///
/// bash and zsh do; dash/POSIX `sh` and fish don't, so callers skip the
/// proctitle and launch with a plain `exec` there (a bad `-a` would fail
/// the launch).
#[must_use]
pub fn shell_supports_exec_a(shell_path: &str) -> bool {
    matches!(
        std::path::Path::new(shell_path).file_name().and_then(|s| s.to_str()),
        Some("bash" | "zsh")
    )
}

/// Build the `exec …` string handed to `sh -c`. Optionally runs the agent
/// under `strace -f -c -o <log>` (`trace`) and/or sets its `argv[0]` via
/// `exec -a <title>` (`proctitle`). Pure — the unit-testable core.
#[must_use]
fn wrap_exec_command(
    cmd: &str,
    trace: Option<&(String, String)>,
    frames: Option<&str>,
    proctitle: Option<&str>,
) -> String {
    let program = match trace {
        Some((tracer, log)) => {
            format!(
                "{} -f -c -o {} {cmd}",
                agent_probe::sh_squote(tracer),
                agent_probe::sh_squote(log)
            )
        }
        None => cmd.to_string(),
    };
    let exec = proctitle.map_or_else(
        || format!("exec {program}"),
        |title| format!("exec -a {} {program}", agent_probe::sh_squote(title)),
    );
    // Frame-timing env is set as a prefix assignment on `exec` (not via an
    // `env` wrapper), so `exec -a <title>` still renames the agent itself
    // and bash exports the vars into the exec'd process. `DEBUG_REPAINTS`
    // rides along so the frame log carries repaint counts.
    match frames {
        None => exec,
        Some(log) => format!(
            "CLAUDE_CODE_FRAME_TIMING_LOG={} CLAUDE_CODE_DEBUG_REPAINTS=1 {exec}",
            agent_probe::sh_squote(log)
        ),
    }
}

/// Install a file-backed logger for the windowed GUI.
///
/// The desktop app launches from a hotkey / `.desktop` entry with no
/// controlling terminal, so `log` records and `eprintln!` are lost —
/// this is why the IME bug "has no log access": the keystroke trace at
/// `terminal.rs` (target `tab_atelier::input_lag`, carrying the IME
/// `key`/`key_char`) is emitted but dropped. Route `log` to
/// `<state>/tab-atelier.log` (append, size-capped, one `.1` rotation)
/// so those records survive a reboot and can be read/tapped like the
/// agent probe — no terminal required.
///
/// No-op unless a filter is configured, so a normal run writes nothing.
/// The filter is resolved in precedence order — `TAB_ATELIER_LOG` env,
/// `RUST_LOG` env, then the persisted [`log_filter_path`] file written by
/// `tab-atelier log …`. The value is a standard `env_logger` filter, e.g.
/// `tab_atelier::input_lag=trace` to capture IME input.
pub fn init_gui_file_logging() {
    let Some(filter) = resolve_log_filter() else {
        return;
    };
    let dir = state_dir(&agent_probe::state_base());
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tab-atelier.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 8 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let mut builder = env_logger::Builder::new();
    builder
        .parse_filters(&filter)
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(file)));
    let logger = builder.build();
    let level = logger.filter();
    log_ring::install(logger, level);
}

/// The effective GUI log filter: `TAB_ATELIER_LOG` env, then `RUST_LOG`
/// env, then the persisted [`log_filter_path`] file. `None` (logging
/// off) when none is set or the persisted value is blank.
#[must_use]
pub fn resolve_log_filter() -> Option<String> {
    std::env::var("TAB_ATELIER_LOG")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .or_else(|| std::fs::read_to_string(log_filter_path()).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Where `tab-atelier log …` persists the log filter, read at GUI
/// startup: `<state>/log.filter`.
#[must_use]
pub fn log_filter_path() -> PathBuf {
    state_dir(&agent_probe::state_base()).join("log.filter")
}

/// Full path of the GUI log file records are routed to.
#[must_use]
pub fn gui_log_path() -> PathBuf {
    state_dir(&agent_probe::state_base()).join("tab-atelier.log")
}

/// Persist the GUI log filter (`Some(filter)`) or clear it (`None`).
/// Takes effect on the next GUI launch. Returns an `io::Error` only on a
/// real filesystem failure (a missing file on clear is success).
///
/// # Errors
/// Propagates create-dir / write / remove failures.
pub fn set_persisted_log_filter(filter: Option<&str>) -> std::io::Result<()> {
    let path = log_filter_path();
    match filter {
        Some(f) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, f.trim())
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

/// Pin the rustls `CryptoProvider` to `ring` at process start.
///
/// Workspace feature unification compiles `rustls` with both `ring`
/// and `aws_lc_rs` enabled (catbus-agent pulls the latter in via
/// reqwest). Without an explicit install,
/// `ServerConfig::builder()` panics: "Could not automatically
/// determine the process-level `CryptoProvider`". Calling
/// `install_default()` here makes TLS startup deterministic.
///
/// Idempotent — second-and-later calls return `Err` (which we ignore)
/// rather than re-installing.
pub fn install_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn try_acquire_single_instance_lock() -> bool {
    use fs2::FileExt;
    let dir = platform::state_base_dir().join(APP_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return true; // can't lock, but don't block startup
    }
    let path = dir.join("tab-atelier.lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return true;
    };
    if file.try_lock_exclusive().is_err() {
        return false;
    }
    // Stash the handle so the lock stays held for the process lifetime.
    let _ = INSTANCE_LOCK.set(file);
    true
}

#[derive(Serialize, Deserialize)]
pub struct TabState {
    /// Stable per-tab UUID. Used by the local API
    /// (`POST /tabs/by-id/{tab_id}/status`) and exported into the
    /// tab's shell as `_TAB_ID` so tools can identify themselves
    /// across rename. Assigned on first creation, persisted across
    /// restarts. `#[serde(default)]` so old tabs.json files generate
    /// a fresh id on first load.
    #[serde(default = "default_tab_id")]
    pub id: String,
    pub name: String,
    pub cwd: Option<String>,
    /// Unix-millis of the last time this tab was genuinely used — desktop
    /// focus, or an explicit web-viewer focus event. Drives the MRU (Ctrl+P /
    /// mobile) ordering; persisted so the ordering survives a restart. Old
    /// tabs.json files without it load as `None` and re-seed on first focus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_wh: Option<f64>,
    /// Cumulative catbus-agent token usage for this tab. Both fields are
    /// zero when no agent session has run yet; skipped entirely in the
    /// serialized file when absent so the common (non-agent) case stays clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    /// `colors_enabled` for this tab — false means the shell was started
    /// with `TERM=dumb` (right-click → Disable colors). Skipped when
    /// `true` so the common case stays out of the serialized file.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub colors_enabled: bool,
    /// Transient agent state — UI hint only, never serialised.
    /// Posted via `POST /tabs/by-id/{id}/status`, cleared by the
    /// staleness sweep after 5 min of no updates.
    #[serde(skip)]
    pub agent_state: Option<AgentStateSnapshot>,
    /// Durable — the last agent session UUID reported on this tab.
    /// Drives auto-resume on next launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// Durable — which agent CLI owns the persisted `agent_session_id`.
    /// Known values: "catbus" (catbus-agent), "claude" (official
    /// Claude Code CLI). Free-form string for future agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// Durable — whether the agent was in plan / read-only mode at
    /// last save. Restored along with the session uuid so auto-resume
    /// brings the tab back into the same mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_plan_mode: Option<bool>,
    /// Durable — this tab IS a session-less daemon (`set-status --kind <verb>
    /// --daemon`): one of our own subcommands run as a tab, like `⛑ brain`.
    /// Restore relaunches it via [`daemon_relaunch_command`] instead of
    /// dropping to a shell. Set by the daemon itself at startup, so an
    /// unrecognised `agent_kind` alone never becomes a command line.
    #[serde(default, skip_serializing_if = "is_false")]
    pub agent_daemon: bool,
    /// Per-tab env vars injected into this tab's PTY (`env set --tab <id>`),
    /// layered ON TOP of the global `tab_env` (per-tab wins). Applied on the
    /// next spawn/respawn of the tab.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub tab_env: std::collections::BTreeMap<String, String>,

    /// Free-form durable labels set from inside the tab
    /// (`tab-atelier set-meta <key> <value>`) and surfaced on `/tabs`.
    ///
    /// Unlike [`Self::tab_env`] it never reaches the PTY and is never masked:
    /// it's labelling, not configuration — a role, a project phase, a
    /// harness's own bookkeeping. We assign no meaning to any key; that's the
    /// point, so an orchestration layer can carry its vocabulary without us
    /// growing a field per idea. Bounded by [`META_MAX_KEYS`] /
    /// [`META_KEY_MAX`] / [`META_VALUE_MAX`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta: std::collections::BTreeMap<String, String>,

    /// Fixed grid size the tab is PINNED to (`tab-atelier resize <tab> --cols N
    /// --rows M`), overriding window-driven sizing so a web viewer isn't
    /// oversized. Both `None` = normal. Persisted so the pin survives a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_rows: Option<u16>,

    /// Per-tab share secrets. Carried in the `?token=` query of share
    /// URLs and validated server-side on the `/tabs/by-id/{uuid}/...`
    /// routes so a read-only link can't be promoted to interactive by
    /// stripping `&ro=1` from the URL (the *token* is the wrong type
    /// for `/input`, not the URL flag). Empty string when not minted;
    /// the API server lazily fills them on first share menu use.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub share_token_rw: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub share_token_ro: String,

    /// Locked tabs refuse every input source: local typing, paste,
    /// hotkeys, remote API (master token included), and share links.
    /// /output and /view still serve; only writes are blocked. Useful
    /// for parking a tab on a long-running command and not nudging it
    /// by accident. Toggled by the right-click menu; persisted across
    /// restarts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,

    /// When true, the tab's shell runs inside a bubblewrap network
    /// namespace (loopback only → no internet). Toggled by the
    /// right-click "Disable internet" menu (GUI) / `net-off` (CLI);
    /// applied on (re)spawn. Persisted so a net-off tab stays off across
    /// restarts. Skipped from JSON when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub net_disabled: bool,

    /// Allowlist-mode config — the tab may reach ONLY these destinations,
    /// enforced by the filtering proxy (and nftables on the headless
    /// service). Mutually exclusive with [`Self::net_disabled`] (full
    /// airgap): when both are set, `net_disabled` wins. All three empty ⇒
    /// the tab is not in allowlist mode. Set via the `net-allow` CLI /
    /// API. See [`Self::net_mode`] / [`Self::allow_set`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net_allow_presets: Vec<crate::net_policy::Preset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net_allow_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net_allow_cidrs: Vec<String>,

    /// Per-tab override of the viewer background color (hex
    /// `#RRGGBB`). When `Some`, beats the global
    /// `Preferences::tab_bg_color`. Set via the right-click "Background
    /// color..." menu (GUI) or `tab-atelier-headless bg-color <tab>
    /// <hex>` (CLI). Skipped from JSON when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg_color: Option<String>,
    /// Per-tab badge override — a short tag drawn on the tab. `None` ⇒ the
    /// tab shows its folder rule's badge, if any. See [`effective_tab_badge`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,

    /// Off-hours auto-lock. When set, the schedule's `(rule, tz)`
    /// pair feeds [`crate::schedule::effective_locked`] alongside the
    /// manual [`Self::locked`] flag. Outside the rule's open windows
    /// every write is refused with 423 and `X-Tab-Locked-Reason:
    /// schedule`. Set via `tab-atelier schedule <tab> "<rule>" --tz
    /// <iana>` (CLI) or the Schedule field in the right-click menu
    /// (GUI). Skipped from JSON when unset so old tabs.json files
    /// stay byte-clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<crate::schedule::TabSchedule>,

    /// Per-tab resource ceilings (memory / CPU / task count). Each
    /// field is optional and, when set, overrides the global
    /// [`Preferences::default_tab_limits`] default for that one axis.
    /// Applied via cgroup v2 on the headless daemon (see
    /// [`crate::cgroup`]); a no-op on platforms / setups without a
    /// delegated cgroup. Skipped from JSON when fully unset.
    #[serde(default, skip_serializing_if = "TabResourceLimits::is_empty")]
    pub limits: TabResourceLimits,

    /// Per-tab ssh-agent. `Some(_)` = the daemon owns a dedicated
    /// `ssh-agent` for this tab and injects its `SSH_AUTH_SOCK` at spawn;
    /// `None` = the tab inherits the ambient environment (today's
    /// behaviour). Toggled via `tab-atelier-headless ssh-agent <tab>` /
    /// `POST /tabs/by-id/{uuid}/ssh-agent`; applied on (re)spawn. Persisted
    /// so the agent is re-provisioned on boot. Skipped from JSON when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_agent: Option<SshAgentConfig>,
}

/// Per-tab ssh-agent configuration (see [`TabState::ssh_agent`]).
///
/// Presence enables a dedicated agent; [`Self::key`] optionally names a
/// passphrase-less private key the daemon auto-loads at spawn. Encrypted
/// keys are the user's job to `ssh-add` inside the tab.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshAgentConfig {
    /// Passphrase-less private key to `ssh-add` at spawn. `None` = start an
    /// empty agent and let the user load keys themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Optional resource ceilings for a tab's process tree.
///
/// Used both as a per-tab override ([`TabState::limits`]) and as the
/// global default ([`Preferences::default_tab_limits`]);
/// [`TabResourceLimits::resolve`] layers the two. Every field is `None`
/// = "no limit on this axis", so the default (all `None`) preserves
/// today's unlimited behaviour.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct TabResourceLimits {
    /// Memory high-water mark, e.g. `"512M"`, `"2G"`, or a bare byte
    /// count. Maps to cgroup `memory.max`. `K`/`M`/`G`/`T` are
    /// 1024-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<String>,
    /// CPU ceiling as a percentage of a single core: `50` = half a
    /// core, `200` = two full cores. Maps to cgroup `cpu.max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<u32>,
    /// Maximum number of tasks (processes + threads) in the tab's
    /// tree. Maps to cgroup `pids.max`. Caps fork bombs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_max: Option<u64>,
}

impl TabResourceLimits {
    /// True when no axis is constrained (the serialised-as-absent case).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.memory_max.is_none() && self.cpu_quota_percent.is_none() && self.tasks_max.is_none()
    }

    /// Resolve effective limits: each axis takes the per-tab value when
    /// set, else falls back to the global default. Mirrors
    /// [`effective_tab_bg`]'s per-tab-over-global policy.
    #[must_use]
    pub fn resolve(per_tab: &Self, global: &Self) -> Self {
        Self {
            memory_max: per_tab.memory_max.clone().or_else(|| global.memory_max.clone()),
            cpu_quota_percent: per_tab.cpu_quota_percent.or(global.cpu_quota_percent),
            tasks_max: per_tab.tasks_max.or(global.tasks_max),
        }
    }

    /// `memory.max` value in bytes, parsed from [`Self::memory_max`].
    /// `K`/`M`/`G`/`T` suffixes are 1024-based; a bare number is bytes.
    /// `None` when unset or unparseable.
    #[must_use]
    pub fn memory_max_bytes(&self) -> Option<u64> {
        parse_memory_bytes(self.memory_max.as_deref()?)
    }

    /// cgroup v2 `cpu.max` line (`"<quota_us> <period_us>"`) for
    /// [`Self::cpu_quota_percent`], using the conventional 100 ms
    /// period. `None` when unset or zero.
    #[must_use]
    pub fn cpu_max_line(&self) -> Option<String> {
        let pct = self.cpu_quota_percent?;
        if pct == 0 {
            return None;
        }
        // period = 100_000 µs; quota = pct% of one core within that.
        Some(format!("{} 100000", u64::from(pct) * 1000))
    }

    /// Apply a partial override: each `Some` axis in `over` replaces the one
    /// here; `None` axes are left untouched. Backs `POST /tabs/<id>/limits`
    /// (and the `tab-atelier limit` CLI) so a client can set just memory
    /// without disturbing cpu/tasks.
    pub fn merge(&mut self, over: &Self) {
        if over.memory_max.is_some() {
            self.memory_max.clone_from(&over.memory_max);
        }
        if over.cpu_quota_percent.is_some() {
            self.cpu_quota_percent = over.cpu_quota_percent;
        }
        if over.tasks_max.is_some() {
            self.tasks_max = over.tasks_max;
        }
    }

    /// `false` when `memory_max` is set but doesn't parse to a byte count — so
    /// the CLI/API can reject a bad value up front instead of silently no-op'ing
    /// at cgroup-write time.
    #[must_use]
    pub fn memory_max_valid(&self) -> bool {
        self.memory_max
            .as_deref()
            .is_none_or(|s| parse_memory_bytes(s).is_some())
    }
}

/// Parse a memory size like `"512M"` / `"2G"` / `"1048576"` into bytes.
/// Suffixes `K`/`M`/`G`/`T` (case-insensitive) are 1024-based. Returns
/// `None` for empty or malformed input.
#[must_use]
fn parse_memory_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let last = s.chars().last()?;
    let (digits, mult) = match last {
        'K' | 'k' => (&s[..s.len() - 1], 1024u64),
        'M' | 'm' => (&s[..s.len() - 1], 1024 * 1024),
        'G' | 'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        'T' | 't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        '0'..='9' => (s, 1),
        _ => return None,
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(n.saturating_mul(mult))
}

/// Parse total physical RAM (bytes) from a `/proc/meminfo` blob — its
/// `MemTotal:` line reports kibibytes (`MemTotal:  65780904 kB`). `None` if the
/// line is missing or unparseable.
#[must_use]
pub fn parse_meminfo_total(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return kb.checked_mul(1024);
        }
    }
    None
}

/// Total physical RAM in bytes, read once from `/proc/meminfo` and cached.
///
/// Total RAM doesn't change at runtime, so the first read is memoized. `None` off
/// Linux or on parse failure. Used as the per-tab RAM-gauge denominator when a
/// tab has no explicit memory cap.
#[must_use]
pub fn system_total_ram_bytes() -> Option<u64> {
    static CACHE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(parse_meminfo_total)
    })
}

/// Default viewer background — Tomorrow Night Blue. Softer than pitch
/// black; legible foreground contrast on most monitors.
pub const DEFAULT_TAB_BG_COLOR: &str = "#002451";

/// Cap on how many [`TabState::meta`] keys one tab can carry. Small on
/// purpose: this is labelling, not storage — anything bigger belongs in a
/// file the agent owns.
pub const META_MAX_KEYS: usize = 16;
/// Cap on a meta key's length.
pub const META_KEY_MAX: usize = 32;
/// Cap on a meta value's length, in chars.
pub const META_VALUE_MAX: usize = 256;

/// Validate one `set-meta` pair, returning the normalised `(key, value)`.
///
/// Keys are lower-cased and restricted to `[a-z0-9_-]` so they stay usable as
/// JSON object keys and as header/CSS-safe identifiers downstream; values are
/// trimmed of control characters (they'd corrupt a header line or a log) and
/// length-capped. An empty value is rejected — a key is deleted by sending a
/// null value on the wire, not by blanking it.
///
/// # Errors
/// A human-readable message naming the rule that failed.
pub fn sanitize_meta(key: &str, value: &str) -> Result<(String, String), String> {
    let key = key.trim().to_ascii_lowercase();
    if key.is_empty() || key.len() > META_KEY_MAX {
        return Err(format!("meta key must be 1..={META_KEY_MAX} chars"));
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("meta key allows only [a-z0-9_-]".to_string());
    }
    let value: String = value.trim().chars().filter(|c| !c.is_control()).collect();
    if value.is_empty() {
        return Err("meta value is empty — pass --clear to remove the key".to_string());
    }
    if value.chars().count() > META_VALUE_MAX {
        return Err(format!("meta value must be at most {META_VALUE_MAX} chars"));
    }
    Ok((key, value))
}

/// Apply one validated meta change: `Some(v)` sets, `None` removes.
///
/// Refuses to grow past [`META_MAX_KEYS`] (updating an existing key always
/// works), so the persisted map stays bounded whatever the API allowed.
pub fn apply_meta_change(map: &mut std::collections::BTreeMap<String, String>, key: &str, value: Option<String>) {
    match value {
        Some(v) if map.len() < META_MAX_KEYS || map.contains_key(key) => {
            map.insert(key.to_string(), v);
        }
        Some(_) => {}
        None => {
            map.remove(key);
        }
    }
}

/// Visual identity attached to a project directory.
///
/// Every tab whose cwd is inside it picks these up. Because a new tab inherits
/// the active tab's cwd, this is also what makes a project's colour survive
/// Ctrl+Shift+T — no settings are copied from tab to tab, they're re-derived
/// from the folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FolderStyle {
    /// `#RRGGBB` background for tabs in this folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Short label shown on the tab (a project tag, an emoji).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
}

/// Cap on a badge's length, in chars — it shares the tab strip with the name.
pub const BADGE_MAX: usize = 6;

/// Validate a badge: trimmed, control characters stripped, length-capped.
///
/// # Errors
/// A human-readable message when it's empty or too long.
pub fn sanitize_badge(badge: &str) -> Result<String, String> {
    let badge: String = badge.trim().chars().filter(|c| !c.is_control()).collect();
    if badge.is_empty() {
        return Err("badge is empty — pass `clear` to remove it".to_string());
    }
    if badge.chars().count() > BADGE_MAX {
        return Err(format!("badge must be at most {BADGE_MAX} chars"));
    }
    Ok(badge)
}

/// The folder rule that applies to `cwd`: the longest configured path that is
/// `cwd` itself or one of its ancestors. `None` when no rule matches.
///
/// Longest-match so a rule on a sub-project (`~/Dev/app/frontend`) refines the
/// one on its parent (`~/Dev/app`) instead of fighting it.
#[must_use]
pub fn folder_style_for<'a>(
    styles: &'a std::collections::BTreeMap<String, FolderStyle>,
    cwd: Option<&str>,
) -> Option<&'a FolderStyle> {
    let cwd = cwd?.trim_end_matches('/');
    styles
        .iter()
        .filter(|(dir, _)| {
            let dir = dir.trim_end_matches('/');
            cwd == dir || (!dir.is_empty() && cwd.starts_with(dir) && cwd.as_bytes().get(dir.len()) == Some(&b'/'))
        })
        .max_by_key(|(dir, _)| dir.trim_end_matches('/').len())
        .map(|(_, style)| style)
}

/// Shown in a stats row whose value the sampler hasn't produced yet.
pub const STAT_PENDING: &str = "—";

/// A stats row's value, or [`STAT_PENDING`] while it is unknown.
///
/// Rows in the right-click menu must exist from the moment it opens, even
/// empty. The power sampler fills in a second or two later, and a row that
/// appears then re-lays out an ALREADY-OPEN menu — which, because the menu can
/// open upward, slides every item under the cursor: a click aimed at "Copy"
/// lands on "Copy all".
#[must_use]
pub fn stat_value(value: Option<String>) -> String {
    value
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| STAT_PENDING.to_string())
}

/// Build the right-click menu's stats rows: one per entry, always.
///
/// A row whose value isn't known yet shows [`STAT_PENDING`] rather than being
/// omitted. The count must depend only on the build's features — never on the
/// data — because the menu is rebuilt every frame while it is open (that's how
/// the numbers stay live), and it can open upward: a row appearing when the
/// sampler answers slides every item above it under a stationary cursor, so a
/// click meant for one entry lands on the next.
#[must_use]
pub fn stats_rows(entries: &[(&str, Option<String>)]) -> Vec<String> {
    entries
        .iter()
        .map(|(label, value)| format!("{label}: {}", stat_value(value.clone())))
        .collect()
}

/// An overlay layer that swallows window input while it is up.
///
/// Every one of these blocks both the Ctrl+P switcher and the right-click
/// context menu (see `AppState::render`'s menu gate), so a layer left open by
/// accident reads as "the mouse and the keyboard stopped working".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    ContextMenu,
    TabSwitcher,
    Renaming,
    HotkeyPicker,
    Qr,
    CloseConfirm,
    ExitConfirm,
    Preferences,
}

/// Which overlay layers are currently up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayState {
    pub context_menu: bool,
    pub tab_switcher: bool,
    pub renaming: bool,
    pub hotkey_picker: bool,
    pub qr: bool,
    pub close_confirm: bool,
    pub exit_confirm: bool,
    pub preferences: bool,
}

/// The layer a root-level Escape should dismiss, or `None` when nothing is up
/// (Escape then belongs to the tab, where vim and friends want it).
///
/// Ordered outermost-visually-first — the transient menu before the modal
/// behind it — so one press closes exactly one layer and a user digging out of
/// a stuck state gets there predictably.
#[must_use]
pub const fn escape_dismisses(s: OverlayState) -> Option<Overlay> {
    if s.context_menu {
        Some(Overlay::ContextMenu)
    } else if s.tab_switcher {
        Some(Overlay::TabSwitcher)
    } else if s.renaming {
        Some(Overlay::Renaming)
    } else if s.hotkey_picker {
        Some(Overlay::HotkeyPicker)
    } else if s.qr {
        Some(Overlay::Qr)
    } else if s.close_confirm {
        Some(Overlay::CloseConfirm)
    } else if s.exit_confirm {
        Some(Overlay::ExitConfirm)
    } else if s.preferences {
        Some(Overlay::Preferences)
    } else {
        None
    }
}

/// A keyboard chord the WINDOW handles, rather than the tab's PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppChord {
    /// Ctrl+P — the MRU tab switcher.
    TabSwitcher,
    /// Ctrl+Shift+T — new tab after the current one.
    NewTab,
    /// Ctrl+Shift+C — copy the selection.
    Copy,
    /// Ctrl+Shift+V — paste.
    Paste,
    /// Alt+Tab — next tab.
    NextTab,
}

/// Match a keystroke to an app chord, or `None` to let the PTY have it.
///
/// Both sides of the dispatch consult this: the terminal view swallows (or
/// acts on) a keystroke that maps to a chord, so the shell never sees `^P`,
/// and the root handler acts once the event bubbles. Two hand-written
/// conditions could drift apart — and a chord the terminal swallowed but the
/// root ignored is a key that silently does nothing.
///
/// The key name is matched **case-insensitively**: with `CapsLock` on, or on a
/// layout that reports the shifted keysym, the same physical chord arrives as
/// `"P"`, and an exact `"p"` comparison drops it on the floor.
#[must_use]
pub fn app_chord(key: &str, ctrl: bool, shift: bool, alt: bool) -> Option<AppChord> {
    let is = |name: &str| key.eq_ignore_ascii_case(name);
    match (ctrl, shift, alt) {
        (true, false, false) if is("p") => Some(AppChord::TabSwitcher),
        (true, true, false) if is("t") => Some(AppChord::NewTab),
        (true, true, false) if is("c") => Some(AppChord::Copy),
        (true, true, false) if is("v") => Some(AppChord::Paste),
        (false, _, true) if is("tab") => Some(AppChord::NextTab),
        _ => None,
    }
}

/// Resolve the effective background color for a tab: per-tab override
/// → folder rule → global pref → Tomorrow Night Blue.
#[must_use]
pub fn effective_tab_bg<'a>(per_tab: Option<&'a str>, folder: Option<&'a str>, global: Option<&'a str>) -> &'a str {
    per_tab.or(folder).or(global).unwrap_or(DEFAULT_TAB_BG_COLOR)
}

/// Resolve the effective badge for a tab: per-tab override → folder rule.
/// `None` ⇒ the tab shows no badge.
#[must_use]
pub fn effective_tab_badge<'a>(per_tab: Option<&'a str>, folder: Option<&'a str>) -> Option<&'a str> {
    per_tab.or(folder)
}

/// The tab's *explicit* tint — per-tab override, else its folder rule.
///
/// Unlike [`effective_tab_bg`] this never falls back to a default: `None`
/// means "never styled", which is what the desktop needs in order to leave the
/// theme's background alone.
#[must_use]
pub fn effective_tab_tint<'a>(per_tab: Option<&'a str>, folder: Option<&'a str>) -> Option<&'a str> {
    per_tab.or(folder)
}

/// Parse `#RRGGBB` into a packed `0xRRGGBB`. `None` for anything else — the
/// same shape the API validator accepts, so a stored colour always renders.
#[must_use]
pub fn parse_hex_rgb(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[must_use]
pub fn default_tab_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Path of the relay token — the credential a relayed claude tab presents to
/// `/relay/anthropic/*`, kept apart from the master `api.token`.
#[must_use]
pub fn relay_token_path() -> PathBuf {
    state_dir(&platform::state_base_dir()).join("relay.token")
}

/// The relay token, minted on first use and persisted 0600.
///
/// Relay mode puts this in every claude tab's `ANTHROPIC_API_KEY`. That value
/// leaks far more easily than a deliberate credential — an agent dumping its
/// environment, a `--debug` transcript, a crash report — so it must not be the
/// master token, which administers every tab. This one does exactly one thing:
/// authenticate to this instance's Anthropic relay. It cannot list tabs, read
/// output, inject input or rotate anything.
///
/// Cached per process; the file is the durable copy, so restarts keep the same
/// value and a remote configured with it keeps working.
#[must_use]
pub fn relay_token() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let path = relay_token_path();
            if let Ok(existing) = std::fs::read_to_string(&path) {
                let existing = existing.trim().to_string();
                if !existing.is_empty() {
                    return existing;
                }
            }
            let minted = mint_share_token();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::write(&path, &minted).is_ok() {
                // Owner-only, same as api.token — a world-readable relay token
                // would hand every local user a free proxy to the account.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
            minted
        })
        .clone()
}

/// 16 random bytes hex-encoded — used for per-tab share secrets.
/// Distinct from the master api.token (which authorises every tab).
#[must_use]
pub fn mint_share_token() -> String {
    use std::fmt::Write as _;
    let mut buf = [0u8; 16];
    platform::random_bytes(&mut buf);
    let mut out = String::with_capacity(32);
    for b in &buf {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

impl Default for TabState {
    fn default() -> Self {
        Self {
            id: default_tab_id(),
            name: String::new(),
            cwd: None,
            last_used_at: None,
            output: None,
            uptime_secs: None,
            energy_wh: None,
            tokens: None,
            colors_enabled: true,
            agent_state: None,
            agent_session_id: None,
            agent_kind: None,
            agent_plan_mode: None,
            agent_daemon: false,
            tab_env: std::collections::BTreeMap::new(),
            meta: std::collections::BTreeMap::new(),
            pinned_cols: None,
            pinned_rows: None,
            share_token_rw: String::new(),
            share_token_ro: String::new(),
            locked: false,
            net_disabled: false,
            net_allow_presets: Vec::new(),
            net_allow_domains: Vec::new(),
            net_allow_cidrs: Vec::new(),
            bg_color: None,
            badge: None,
            schedule: None,
            limits: TabResourceLimits::default(),
            ssh_agent: None,
        }
    }
}

impl TabState {
    /// Resolve the persisted fields into the three-state network mode.
    /// `net_disabled` (full airgap) wins over any allowlist config.
    #[must_use]
    pub const fn net_mode(&self) -> crate::net_policy::NetMode {
        use crate::net_policy::NetMode;
        if self.net_disabled {
            NetMode::Off
        } else if self.net_allow_presets.is_empty()
            && self.net_allow_domains.is_empty()
            && self.net_allow_cidrs.is_empty()
        {
            NetMode::On
        } else {
            NetMode::Allowlist
        }
    }

    /// Flatten the allowlist config into the resolved match-set the proxy /
    /// nftables consume. Empty when not in allowlist mode.
    #[must_use]
    pub fn allow_set(&self) -> crate::net_policy::AllowSet {
        crate::net_policy::AllowSet::build(&self.net_allow_presets, &self.net_allow_domains, &self.net_allow_cidrs)
    }

    /// The raw allowlist inputs, carried into the spawn paths.
    #[must_use]
    pub fn allow_config(&self) -> crate::net_policy::AllowConfig {
        crate::net_policy::AllowConfig {
            presets: self.net_allow_presets.clone(),
            domains: self.net_allow_domains.clone(),
            cidrs: self.net_allow_cidrs.clone(),
        }
    }
}

/// Discrete agent runtime states a tool inside a tab can publish via
/// `POST /tabs/by-id/{id}/status`. Drives the desktop LED colour and
/// the share-link viewer's tab-title badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Thinking,
    Waiting,
    Error,
}

/// In-memory snapshot stored on each `TabState`.
///
/// Carries the state plus an optional short label ("tool: Bash")
/// and the wall-clock at which it was reported, so the staleness
/// sweep can evict abandoned indicators.
#[derive(Clone, Debug)]
pub struct AgentStateSnapshot {
    pub state: AgentState,
    pub label: Option<String>,
    pub updated_at: std::time::Instant,
}

/// Wall-clock milliseconds since the Unix epoch (`0` if the clock predates it).
///
/// Used to stamp `last_used_at` on tabs so any client can sort the list
/// most-recently-used-first without keeping its own recency map.
#[must_use]
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// How long after the last PTY output a tab's LED stays green ("working").
///
/// A `--resume`d agent streams its reply with no thinking hook, so fresh output
/// is what keeps the dot green. Shared by the desktop renderer and the headless
/// snapshot builder so both apply the identical window.
pub const STREAMING_LED_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// The per-tab agent "LED" — the single colored dot drawn left of a tab name.
///
/// Shared by the desktop tab strip (`app.rs`), the `/tabs` JSON (`led` field)
/// and the mobile remote, so all three render the identical indicator.
/// Precedence, highest first: `Dead` > `Error` > `Working` > `Unreviewed` >
/// `Idle`. Derived by [`compute_tab_led`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabLed {
    /// Session anchored but the agent process is gone — needs relaunch.
    Dead,
    /// Agent reported an error.
    Error,
    /// Thinking, or fresh PTY output within the last few seconds.
    Working,
    /// Agent worked then stopped, not yet reviewed.
    Unreviewed,
    /// Session attached, nothing to review.
    Idle,
}

impl TabLed {
    /// Stable lowercase slug for the `/tabs` JSON `led` field.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Dead => "dead",
            Self::Error => "error",
            Self::Working => "working",
            Self::Unreviewed => "unreviewed",
            Self::Idle => "idle",
        }
    }

    /// Exact dot color `(r, g, b)` in `0.0..=1.0` sRGB — the SINGLE source of
    /// truth for every renderer. The desktop passes it straight to gpui's
    /// `Rgba`; the mobile remote mirrors these same values (it's a disjoint
    /// crate and can't import this one).
    #[must_use]
    pub const fn rgb(self) -> (f32, f32, f32) {
        match self {
            Self::Dead => (0.55, 0.16, 0.16),       // dim red  #8C2929
            Self::Error => (0.937, 0.267, 0.267),   // red      #EF4444
            Self::Working => (0.306, 0.788, 0.690), // green    #4EC9B0
            Self::Unreviewed => (0.36, 0.60, 1.0),  // blue     #5C99FF
            Self::Idle => (0.45, 0.45, 0.45),       // grey     #737373
        }
    }
}

/// Derive the per-tab LED from raw agent signals.
///
/// The one implementation of the desktop tab strip's dot precedence and
/// visibility gate, reused by the `/tabs` serializer so the CLI viewer and
/// mobile remote show the identical LED. Returns `None` when no dot should be
/// drawn (the desktop's `agent_led_visible` gate).
///
/// - `agent_state`: transient hook state, if any.
/// - `session_attached`: a durable agent kind is set (`agent_kind.is_some()`).
/// - `is_brain`: the attached kind is the auto-injector `"brain"` (exempt from
///   the dead check — it has no long-lived process).
/// - `agent_alive`: the agent process is present (catbus liveness). Pass `true`
///   where liveness isn't tracked, so `Dead` never triggers.
/// - `full_sweep_ran`: at least one liveness sweep has completed (avoids a
///   false `Dead` before the first probe).
/// - `unreviewed_work`: agent worked then stopped and hasn't been reviewed.
/// - `recent_output`: fresh PTY output within the streaming window.
#[must_use]
#[allow(clippy::fn_params_excessive_bools)]
pub const fn compute_tab_led(
    agent_state: Option<AgentState>,
    session_attached: bool,
    is_brain: bool,
    agent_alive: bool,
    full_sweep_ran: bool,
    unreviewed_work: bool,
    recent_output: bool,
) -> Option<TabLed> {
    let agent_dead = session_attached && !agent_alive && !is_brain && full_sweep_ran;
    // Visibility gate (app.rs::agent_led_visible): dead, or a transient state
    // exists, or a session is attached and it's alive-or-unreviewed.
    let visible = agent_dead || agent_state.is_some() || (session_attached && (agent_alive || unreviewed_work));
    if !visible {
        return None;
    }
    let working = matches!(agent_state, Some(AgentState::Thinking)) || recent_output;
    Some(if agent_dead {
        TabLed::Dead
    } else if matches!(agent_state, Some(AgentState::Error)) {
        TabLed::Error
    } else if working {
        TabLed::Working
    } else if unreviewed_work {
        TabLed::Unreviewed
    } else {
        TabLed::Idle
    })
}

const fn default_true() -> bool {
    true
}
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Serialize, Deserialize)]
pub struct SavedState {
    pub tabs: Vec<TabState>,
    pub active: usize,
    /// `true` when the user had toggled "Windowed mode" (Guake-style drop-down
    /// is the default, hence the field's name in the negative). Skipped when
    /// `false` so an unchanged session stays out of the serialized file.
    #[serde(default, skip_serializing_if = "is_false")]
    pub windowed: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

#[must_use]
pub fn state_dir(base: &std::path::Path) -> PathBuf {
    base.join(APP_DIR)
}

/// Sub-directory that holds the global tab list and preferences,
/// underneath `config_base_dir()` (e.g. `~/.local/tab-atelier`).
#[must_use]
pub fn config_dir(base: &std::path::Path) -> PathBuf {
    base.join(APP_DIR)
}

#[must_use]
pub fn state_path(base: &std::path::Path) -> PathBuf {
    state_dir(base).join("tabs.json")
}

#[must_use]
pub fn config_state_path(config_base: &std::path::Path) -> PathBuf {
    config_dir(config_base).join("tabs.json")
}

/// CRC32 (IEEE) — small inline implementation; used to disambiguate tab
/// names whose sanitized form would otherwise collide (e.g. `foo/bar` and
/// `foo_bar` both sanitize to `foo_bar`).
/// CRC32 lookup table (IEEE polynomial, reflected). Built once on
/// first use. A table-driven CRC is ~8x fewer inner operations than
/// the bit-by-bit form and this runs on every API response `ETag`,
/// every `/output` poll, and every persist tick.
static CRC32_TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();

fn crc32_table() -> &'static [u32; 256] {
    CRC32_TABLE.get_or_init(|| {
        const POLY: u32 = 0xEDB8_8320;
        let mut table = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                let mask = (c & 1).wrapping_neg();
                c = (c >> 1) ^ (POLY & mask);
                k += 1;
            }
            table[n] = c;
            n += 1;
        }
        table
    })
}

#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc: u32 = !0;
    for &b in data {
        crc = (crc >> 8) ^ table[((crc ^ u32::from(b)) & 0xff) as usize];
    }
    !crc
}

/// Sanitize a tab name into something safe to use as a filename component
/// and append a CRC32 of the original name so two tabs whose sanitized
/// forms collide still get distinct files.
///
/// Non-alphanumeric and non-`._-` characters become `_`. Result is bounded
/// in length so very long names don't blow past OS path limits.
#[must_use]
pub fn sanitize_tab_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    if out.len() > 100 {
        out.truncate(100);
    }
    let hash = crc32(name.as_bytes());
    format!("{out}-{hash:08x}")
}

#[must_use]
pub fn tab_output_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("output_tab-{}.json", sanitize_tab_filename(tab_name)))
}

#[must_use]
pub fn tab_power_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("power_tab-{}.json", sanitize_tab_filename(tab_name)))
}

#[must_use]
pub fn tab_uptime_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("uptime_tab-{}.json", sanitize_tab_filename(tab_name)))
}

pub fn save_tab_uptime(state_base: &std::path::Path, tab_name: &str, uptime_secs: f64) {
    let dir = state_dir(state_base);
    let path = tab_uptime_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &uptime_secs, false);
}

#[must_use]
pub fn load_tab_uptime(state_base: &std::path::Path, tab_name: &str) -> Option<f64> {
    load_f64_with_bak(&tab_uptime_path(state_base, tab_name))
}

/// Single background thread that serialises persist's small state
/// writes off the input-latency-critical thread.
///
/// Covers tabs.json and the per-tab uptime / energy / token files:
/// every one of those writes ends in an `fsync`, and an fsync on a
/// busy disk stalls for tens of milliseconds — a keystroke landing
/// mid-persist used to freeze until it finished (issue #9). One
/// thread, FIFO, so successive writes to the same path keep their
/// order (last submit wins on disk).
///
/// Shutdown paths write synchronously instead; call [`Self::flush`]
/// FIRST so a periodic write queued moments earlier can't land after
/// (and clobber) the final state.
pub struct StateWriter {
    tx: std::sync::mpsc::Sender<StateWriteJob>,
}

enum StateWriteJob {
    Run(Box<dyn FnOnce() + Send>),
    Flush(std::sync::mpsc::Sender<()>),
}

impl StateWriter {
    #[must_use]
    pub fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<StateWriteJob>();
        let _ = std::thread::Builder::new().name("state-writer".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    StateWriteJob::Run(f) => f(),
                    StateWriteJob::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self { tx }
    }

    /// Queue one write. The closure owns everything it needs (paths,
    /// serialized bytes) — the caller's dedup state is updated at
    /// submit time, exactly as it was when the write ran inline.
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.tx.send(StateWriteJob::Run(Box::new(job)));
    }

    /// Block until every previously-submitted write has completed.
    pub fn flush(&self) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if self.tx.send(StateWriteJob::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }
}

pub fn save_tab_energy(state_base: &std::path::Path, tab_name: &str, energy_wh: f64) {
    let dir = state_dir(state_base);
    let path = tab_power_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &energy_wh, false);
}

#[must_use]
pub fn load_tab_energy(state_base: &std::path::Path, tab_name: &str) -> Option<f64> {
    load_f64_with_bak(&tab_power_path(state_base, tab_name))
}

/// Cumulative token usage for one tab's catbus-agent session.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

#[must_use]
pub fn tab_tokens_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("tokens_tab-{}.json", sanitize_tab_filename(tab_name)))
}

pub fn save_tab_tokens(state_base: &std::path::Path, tab_name: &str, usage: &TokenUsage) {
    let dir = state_dir(state_base);
    let path = tab_tokens_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, usage, false);
}

#[must_use]
pub fn load_tab_tokens(state_base: &std::path::Path, tab_name: &str) -> Option<TokenUsage> {
    let path = tab_tokens_path(state_base, tab_name);
    if let Ok(data) = std::fs::read_to_string(&path)
        && let Ok(v) = serde_json::from_str::<TokenUsage>(&data)
    {
        return Some(v);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(v) = serde_json::from_str::<TokenUsage>(&data)
    {
        return Some(v);
    }
    None
}

fn load_f64_with_bak(path: &std::path::Path) -> Option<f64> {
    if let Ok(data) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<f64>(&data)
    {
        return Some(v);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(v) = serde_json::from_str::<f64>(&data)
    {
        return Some(v);
    }
    None
}

#[must_use]
pub fn load_state_from(base: &std::path::Path) -> Option<SavedState> {
    load_state_at(&state_path(base))
}

/// Hard cap on the size of a state JSON file we'll read into memory.
/// `tabs.json` is metadata for a handful of tabs — a few KB in
/// practice. A multi-GB file (corruption, or a hostile local write)
/// must not be slurped whole and OOM the daemon at startup.
const MAX_STATE_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Read a state file, refusing anything larger than
/// [`MAX_STATE_FILE_BYTES`] without reading it into memory.
fn read_state_file_capped(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_STATE_FILE_BYTES {
        log::warn!(
            "state file {} is {} bytes (> {} cap) — refusing to load",
            path.display(),
            meta.len(),
            MAX_STATE_FILE_BYTES
        );
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[must_use]
pub fn load_state_at(path: &std::path::Path) -> Option<SavedState> {
    if let Some(data) = read_state_file_capped(path)
        && let Ok(state) = serde_json::from_str::<SavedState>(&data)
    {
        return Some(state);
    }
    // Primary file missing or corrupt — try rotated backups, newest first.
    for ext in ["bak", "bak.1", "bak.2"] {
        let alt = path.with_extension(format!("json.{ext}"));
        if let Some(data) = read_state_file_capped(&alt)
            && let Ok(state) = serde_json::from_str::<SavedState>(&data)
        {
            log::warn!("loaded state from backup {}", alt.display());
            return Some(state);
        }
    }
    None
}

/// Load tab list and hydrate each tab's output / uptime / energy from its
/// per-tab file under `state_base`.
///
/// Per-tab hydration fans out over a few threads: each tab is up to ~8
/// independent file probes (each metric retries a `.bak` on miss) plus a
/// JSON parse of a possibly multi-hundred-KB output string, and it all
/// runs before the first paint — serially, a 60-tab startup paid ~480
/// opens + every parse back to back on one core.
#[must_use]
pub fn load_state_with_outputs(config_base: &std::path::Path, state_base: &std::path::Path) -> Option<SavedState> {
    let mut state = load_state_at(&config_state_path(config_base))?;
    let hydrate = |t: &mut TabState| {
        if t.output.is_none() {
            t.output = load_tab_output(state_base, &t.name);
        }
        if t.uptime_secs.is_none() {
            t.uptime_secs = load_tab_uptime(state_base, &t.name);
        }
        if t.energy_wh.is_none() {
            t.energy_wh = load_tab_energy(state_base, &t.name);
        }
        if t.tokens.is_none() {
            t.tokens = load_tab_tokens(state_base, &t.name);
        }
    };
    // Scoped threads over disjoint chunks; the files are independent.
    // A handful of workers is plenty — this is I/O + JSON parsing, and
    // more threads just fight over the page cache.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    if state.tabs.len() <= 1 || workers <= 1 {
        state.tabs.iter_mut().for_each(hydrate);
    } else {
        let chunk = state.tabs.len().div_ceil(workers);
        std::thread::scope(|s| {
            for tabs in state.tabs.chunks_mut(chunk) {
                s.spawn(|| tabs.iter_mut().for_each(hydrate));
            }
        });
    }
    Some(state)
}

#[derive(Debug, Clone)]
pub struct FontConfig {
    pub family: String,
    pub weight: u16,
    pub size: f32,
    pub scroll_sensitivity: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            weight: 400,
            size: 16.0,
            scroll_sensitivity: 1.0,
        }
    }
}

#[must_use]
pub fn load_font_config(config_base: &std::path::Path) -> FontConfig {
    let config_path = config_base.join("zed/settings.json");
    load_font_config_from(&config_path)
}

#[must_use]
pub fn load_font_config_from(path: &std::path::Path) -> FontConfig {
    let mut config = FontConfig::default();

    let Ok(data) = std::fs::read_to_string(path) else {
        return config;
    };

    let stripped: String = strip_json_comments(&data);

    let Ok(parsed): Result<serde_json::Value, _> = serde_json::from_str(&stripped) else {
        return config;
    };

    if let Some(family) = parsed.get("ui_font_family").and_then(|v| v.as_str()) {
        config.family = family.to_string();
    }
    if let Some(weight) = parsed.get("ui_font_weight").and_then(serde_json::Value::as_u64) {
        config.weight = weight as u16;
    }
    if let Some(size) = parsed.get("ui_font_size").and_then(serde_json::Value::as_f64) {
        config.size = size as f32;
    } else if let Some(size) = parsed.get("buffer_font_size").and_then(serde_json::Value::as_f64) {
        config.size = size as f32;
    }
    if let Some(sens) = parsed.get("scroll_sensitivity").and_then(serde_json::Value::as_f64) {
        config.scroll_sensitivity = (sens as f32).max(0.01);
    }

    config
}

/// Resolve the GUI terminal font in the priority order the user asked
/// for: **preferences.json → zed `settings.json` → fontconfig**.
///
/// The generic "monospace" string is only a last resort — gpui resolves
/// it to a font with a too-wide cell advance on some systems (the
/// "horribly spaced" look), so when nothing more specific is set we ask
/// `fc-match` for the concrete family it maps to.
#[must_use]
pub fn resolve_font_config(config_base: &std::path::Path, prefs: &Preferences) -> FontConfig {
    // Tier 2: zed/settings.json (falls back to the "monospace" default).
    let mut config = load_font_config(config_base);

    // Tier 1: preferences.json wins outright when set.
    if let Some(family) = prefs.font_family.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        config.family = family.to_string();
    }
    if let Some(size) = prefs.font_size.filter(|s| *s > 0.0) {
        config.size = size;
    }

    // Tier 3: still on the generic alias ⇒ resolve it via fontconfig.
    if config.family.trim().eq_ignore_ascii_case("monospace")
        && let Some(concrete) = fc_match_monospace()
    {
        config.family = concrete;
    }
    config
}

/// Ask fontconfig which concrete family the generic "monospace" alias
/// maps to (e.g. `DejaVu Sans Mono`). `None` when `fc-match` is absent
/// (non-Linux / minimal container) or yields nothing useful.
fn fc_match_monospace() -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{family}", "monospace"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    // fc-match can return a comma list ("Fam A,Fam B") — take the first.
    let first = raw.split(',').next().unwrap_or("").trim();
    if first.is_empty() || first.eq_ignore_ascii_case("monospace") {
        None
    } else {
        Some(first.to_string())
    }
}

fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    out.push(next);
                    chars.next();
                }
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if ch == '/' {
            match chars.peek() {
                Some(&'/') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some(&'*') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[must_use]
pub fn load_wakatime_key(config_base: &std::path::Path) -> Option<String> {
    let config_path = config_base.join("zed/settings.json");
    let data = std::fs::read_to_string(config_path).ok()?;
    let stripped = strip_json_comments(&data);
    let parsed: serde_json::Value = serde_json::from_str(&stripped).ok()?;
    parsed
        .get("wakatime")
        .and_then(|w| w.get("settings"))
        .and_then(|s| s.get("api-key"))
        .and_then(|k| k.as_str())
        .map(std::string::ToString::to_string)
}

#[derive(Serialize, Deserialize, Default)]
pub struct Preferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Cursor shape id (`block` / `bar` / `underline`, see
    /// [`crate::theme::CursorStyle`]). `None` → block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<u8>,
    /// Show a tiny per-tab RAM gauge in the tab bar (#28 V2 / S5). Off by
    /// default — the tab bar can hold 30+ tabs, so the gauge stays visually
    /// cheap and opt-in. Toggled from a tab's right-click menu.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_tab_gauge: bool,
    /// Force "Claude-only" mode: every new tab launches `claude` in `auto`
    /// mode instead of a shell. Toggled from the right-click menu (the
    /// "New bash tab" item cancels it). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claude_only: bool,
    /// Relay mode: forward claude tabs' Anthropic API calls through the remote
    /// tab-atelier named by `relay_endpoint_id`. Off by default. See
    /// [`crate::RELAY_MODE`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relay_mode: bool,
    /// Which configured `remote_endpoints` entry (by `id`) to relay through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_endpoint_id: Option<String>,
    /// Egress role: this instance accepts `/relay/anthropic/*` and forwards it
    /// to `api.anthropic.com` using its own Claude login. Set on the REMOTE.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relay_egress: bool,
    /// Global env vars injected into every tab's PTY (`env set --global`).
    /// Mirrored into [`crate::TAB_ENV_GLOBAL`] at startup.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub tab_env: std::collections::BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "deserialize_hotkeys",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hotkeys: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_editor: Option<String>,
    /// `addr:port` of the plain-HTTP API listener. Defaults to
    /// `0.0.0.0:7890`. Set to `127.0.0.1:N` to restrict to loopback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_addr: Option<String>,

    /// `addr:port` of the TLS API listener (self-signed cert).
    /// Defaults to `0.0.0.0:7891`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_tls_addr: Option<String>,

    /// Path to a user-supplied TLS certificate (PEM). When set
    /// **with** `api_tls_key_path` the daemon serves this cert
    /// instead of generating a self-signed one — the typical case
    /// is a Cloudflare Origin certificate (`origin.pem`) put behind
    /// a Cloudflare Tunnel / Origin Pull. Multi-cert PEMs (leaf +
    /// intermediate) are loaded as a chain so clients that don't
    /// trust the issuing CA can still build a path. Renewal is the
    /// operator's responsibility — we never touch a file we don't
    /// own. Leave unset (or unpaired with the key) to fall back to
    /// the self-signed `tls.crt` in the state dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_tls_cert_path: Option<String>,

    /// Path to the matching PEM private key for `api_tls_cert_path`.
    /// Either both keys are set or neither — a half-configured pair
    /// is treated as "not configured" and the daemon falls back to
    /// the self-signed cert with a startup warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_tls_key_path: Option<String>,

    /// Path to a PEM bundle of CA certs to authenticate INCOMING
    /// client certificates against (mutual TLS). When set, every TLS
    /// request must present a client cert that chains to one of
    /// these CAs — typically the Cloudflare Authenticated Origin Pull
    /// root from
    /// `https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem`,
    /// so the origin only accepts traffic that came through CF.
    /// Unset ⇒ no client-cert check (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_tls_client_ca_path: Option<String>,

    /// Default terminal background color (hex `#RRGGBB`). Applied
    /// in the share-link xterm.js viewer; per-tab override lives on
    /// `TabState::bg_color` and wins when set. None ⇒ falls back to
    /// the Tomorrow Night Blue default (`#002451`) which is softer
    /// on the eyes than pure black.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_bg_color: Option<String>,
    /// Per-project visual identity: absolute path → colour/badge for every tab
    /// whose cwd is inside it. Mirrored into [`crate::FOLDER_STYLES`] at
    /// startup; a per-tab override still wins. See [`folder_style_for`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub folder_styles: std::collections::BTreeMap<String, FolderStyle>,

    /// Default network allowlist applied to **newly created** tabs
    /// (presets / domains / CIDRs). Empty ⇒ new tabs start unrestricted.
    /// Existing tabs keep whatever they were configured with. Set via
    /// `net-default` (CLI) — see [`Self::default_allow_config`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_net_allow_presets: Vec<crate::net_policy::Preset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_net_allow_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_net_allow_cidrs: Vec<String>,

    /// Headless PTY dimensions. The GUI re-sizes its terminals from
    /// the window, but the headless daemon has no display — the
    /// alacritty PTY stays at whatever it spawned with. Default is
    /// 80×24, which is too narrow for modern TUIs (Claude Code etc.)
    /// and makes the share-link viewer at xterm.js look cramped.
    /// Tune via `tab-atelier-headless ports --pty-cols 200 --pty-rows 50`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_rows: Option<u16>,

    /// GUI terminal font family. Highest-priority source for the
    /// font (over `zed/settings.json`'s `ui_font_family`). Set this to
    /// a concrete installed monospace (e.g. `JetBrains Mono`, `DejaVu
    /// Sans Mono`) — the generic "monospace" default can resolve to a
    /// font with a too-wide advance, giving the "horribly spaced"
    /// look. Unset ⇒ zed settings ⇒ fontconfig-resolved monospace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,

    /// GUI terminal font size in px. Overrides `zed/settings.json`'s
    /// `ui_font_size` / `buffer_font_size`. Unset ⇒ those ⇒ 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,

    /// Public base URL for share links — when set, the "Copy share
    /// link" menu emits `<this>/tabs/by-id/<uuid>/view?...` instead
    /// of `http://<LAN-IP>:<port>/...`. Useful when the API is
    /// reverse-proxied (Caddy, nginx) under a public hostname so
    /// recipients can reach the share without VPN'ing into the LAN.
    /// No trailing slash; leave unset to use the LAN URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_url_base: Option<String>,

    /// Saved remote `tab-atelier-headless` endpoints the GUI can
    /// mirror tabs from. Each entry carries its own bearer token +
    /// TOFU-pinned cert fingerprint. The list is allowed to be empty
    /// (the common case for users who only run the local instance).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_endpoints: Vec<RemoteEndpoint>,

    /// Global default per-tab resource ceilings, applied to every tab
    /// whose own [`TabState::limits`] leaves an axis unset. Each axis is
    /// optional; all unset (the default) keeps tabs unlimited as before.
    /// Headless-only (needs a delegated cgroup); set in
    /// `preferences.json`, e.g.
    /// `"default_tab_limits": {"memory_max": "1G", "tasks_max": 512}`.
    #[serde(default, skip_serializing_if = "TabResourceLimits::is_empty")]
    pub default_tab_limits: TabResourceLimits,

    /// Spawn every tab's shell in a cleared environment (PHP-FPM
    /// `clear_env = yes` style): only the curated [`minimal_pty_env`]
    /// allowlist (PATH, HOME, USER/LOGNAME, SHELL, locale, TZ, colours,
    /// the tab API vars and the telemetry opt-out) reaches the shell;
    /// everything else from the desktop session — `DISPLAY`,
    /// `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, `*_TOKEN`, … — is
    /// dropped. Off by default; opt in when you want tabs isolated from
    /// the launching environment. `None`/absent ⇒ `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_env: Option<bool>,

    /// User-defined `key=value` pairs injected into every tab when
    /// `clear_env` is on. Layered on top of the kept system basics and
    /// colours, and **win on key conflicts** (set `PATH`, `EDITOR`,
    /// `LANG`, … to your own values here). The per-tab API vars and the
    /// telemetry opt-out are applied after these and stay fixed. Ignored
    /// when `clear_env` is off (the tab inherits the full parent env
    /// then). Example in `preferences.json`:
    /// `"clear_env_vars": {"EDITOR": "vim", "PATH": "/opt/bin:/usr/bin"}`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub clear_env_vars: std::collections::BTreeMap<String, String>,
}

impl Preferences {
    /// The default allowlist for new tabs, as an [`crate::net_policy::AllowConfig`].
    /// Empty when no default is configured.
    #[must_use]
    pub fn default_allow_config(&self) -> crate::net_policy::AllowConfig {
        crate::net_policy::AllowConfig {
            presets: self.default_net_allow_presets.clone(),
            domains: self.default_net_allow_domains.clone(),
            cidrs: self.default_net_allow_cidrs.clone(),
        }
    }
}

/// One persisted remote `tab-atelier-headless` instance the desktop
/// GUI can mirror tabs from. Stored under `Preferences::remote_endpoints`
/// in `preferences.json`.
///
/// The `cert_sha256` is filled in by the "Pin certificate" flow in the
/// Preferences dialog (trust-on-first-use). The `token` mirrors the
/// bearer token from the remote's `api.token` file.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RemoteEndpoint {
    /// Local UUID v4 — used as a stable key across renames of the
    /// `label` field. Generated on first save.
    pub id: String,
    /// Short human-friendly label rendered in the tab badge
    /// ("colossus", "build-box"). Free-form.
    pub label: String,
    /// Full base URL of the remote API. Either `http://host:port`
    /// (plain) or `https://host:port` (TLS — `cert_sha256` is then
    /// required).
    pub url: String,
    /// Bearer token. Mirrors the remote's `~/.local/state/tab-atelier/api.token`.
    /// Full API access: the sidecar (`remote attach` / `put` / `get`) needs it
    /// to list tabs, send input and move files.
    pub token: String,
    /// Bearer token for the Anthropic relay hop only, mirroring the remote's
    /// `relay.token`. Separate from [`Self::token`] because the relay route
    /// refuses the master token by design — see [`relay_credential`]. Empty
    /// when the endpoint isn't used for relaying.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub relay_token: String,
    /// Hex SHA-256 of the remote's TLS cert (TOFU-pinned).
    pub cert_sha256: String,
    /// When true, the GUI connects to this endpoint at startup
    /// instead of waiting for an explicit "Connect" click.
    #[serde(default)]
    pub autoconnect: bool,
    /// Cloudflare Access service-token pair. When both are set, every request
    /// (HTTP + WebSocket upgrade) carries them as `CF-Access-Client-Id` /
    /// `CF-Access-Client-Secret` so a remote behind Cloudflare Zero Trust
    /// authorizes the sidecar without an interactive browser login. Empty when
    /// the endpoint isn't behind Access.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cf_access_client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cf_access_client_secret: String,
}

impl RemoteEndpoint {
    /// True when a Cloudflare Access service token is configured (both halves
    /// present) — callers add the `CF-Access-Client-*` headers to each request.
    #[must_use]
    pub const fn has_cf_service_token(&self) -> bool {
        !self.cf_access_client_id.is_empty() && !self.cf_access_client_secret.is_empty()
    }
}

pub const DEFAULT_API_PORT: u16 = 7890;
/// Plaintext HTTP API bind — loopback-only by default.
///
/// It carries the master bearer token in clear and is what in-tab
/// tools reach via `http://127.0.0.1:7890`. Binding it to `0.0.0.0`
/// would let anyone on the LAN sniff/replay the token, so LAN exposure
/// must be an explicit opt-in via preferences. The TLS listener below
/// is the supported way to reach the API from another host (e.g. the
/// mobile remote).
pub const DEFAULT_API_ADDR: &str = "127.0.0.1:7890";
/// TLS API bind. Stays on all interfaces so the mobile remote / share
/// links keep working over the LAN, but the traffic is encrypted and
/// the token never crosses the wire in clear.
pub const DEFAULT_API_TLS_ADDR: &str = "0.0.0.0:7891";

/// System-wide preferences file shipped by the .deb as a dpkg conffile.
///
/// `load_preferences()` reads this as a fallback when the per-user
/// file is absent or unparsable, so an admin can set defaults (bind
/// addresses, relay address) without each user having to create
/// their own `preferences.json`. Per-user settings always win.
pub const SYSTEM_PREFERENCES_PATH: &str = "/etc/tab-atelier/preferences.json";

/// Hex-encoded SHA-256 of a remote's TLS cert, captured without
/// validating anything (trust-on-first-use).
///
/// Used by the Preferences "Pin certificate" button to fill the
/// `cert_sha256` field on a `RemoteEndpoint`. This is intentionally
/// NOT a security check — it accepts any cert the server offers. The
/// fingerprint becomes load-bearing only once the user saves the
/// endpoint and subsequent connections compare against it.
///
/// Errors come back as plain strings so callers can render them in a
/// toast.
///
/// # Errors
///
/// Returns `Err` when the URL can't be parsed, the TCP connect fails,
/// the TLS handshake never reaches the certificate stage, or the
/// server presents no certificate.
pub fn fetch_cert_fingerprint(url: &str) -> Result<String, String> {
    use sha2::Digest;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    let (host, port) = parse_https_host_port(url)?;
    let server_name =
        rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| format!("invalid host {host:?}: {e}"))?;

    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let verifier = Arc::new(CertCapturingVerifier {
        captured: captured.clone(),
    });

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let mut conn =
        rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|e| format!("rustls client init: {e}"))?;

    let mut sock = std::net::TcpStream::connect((host.as_str(), port)).map_err(|e| format!("tcp connect: {e}"))?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| format!("set read timeout: {e}"))?;
    sock.set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| format!("set write timeout: {e}"))?;

    // Drive the handshake until the verifier has captured the cert
    // (which happens as part of the server's ServerHello / Certificate
    // exchange). We don't care whether the handshake "succeeds" past
    // that point — TOFU pinning doesn't validate.
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);
    let _ = stream.flush();
    if captured.lock().is_ok_and(|g| g.is_none()) {
        // Send a minimal probe to nudge the handshake forward if
        // flush() returned before the certificate arrived.
        let _ = stream.write_all(b"GET / HTTP/1.0\r\n\r\n");
        let mut buf = [0u8; 1];
        let _ = stream.read(&mut buf);
    }

    let der = captured
        .lock()
        .map_err(|_| "cert capture mutex poisoned".to_string())?
        .clone()
        .ok_or_else(|| "server presented no certificate".to_string())?;

    let digest = sha2::Sha256::digest(&der);
    Ok(hex_encode(&digest))
}

fn parse_https_host_port(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("expected https:// URL, got {url:?}"))?;
    // Strip path/query if present.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: [::1]:7891
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated IPv6 in {url:?}"))?;
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| format!("missing port after IPv6 literal in {url:?}"))?;
        (h.to_string(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_string(), p)
    } else {
        (authority.to_string(), "443")
    };
    let port = port.parse::<u16>().map_err(|e| format!("bad port {port:?}: {e}"))?;
    Ok((host, port))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug)]
struct CertCapturingVerifier {
    captured: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl rustls::client::danger::ServerCertVerifier for CertCapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Ok(mut g) = self.captured.lock() {
            *g = Some(end_entity.as_ref().to_vec());
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn deserialize_hotkeys<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let raw: Vec<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
            serde_json::Value::String(s) => legacy_hotkey_id_to_keycode(&s),
            _ => None,
        })
        .collect())
}

fn legacy_hotkey_id_to_keycode(id: &str) -> Option<u8> {
    match id {
        "grave" => Some(49),
        "f1" => Some(67),
        "f11" => Some(95),
        "f12" => Some(96),
        "xf86calculator" => Some(148),
        _ => None,
    }
}

pub static DEFAULT_HOTKEYS: &[u8] = &[49, 148];

struct KeycodeInfo {
    keycode: u8,
    label: &'static str,
    gpui_key: &'static str,
}

static KEYCODE_TABLE: &[KeycodeInfo] = &[
    KeycodeInfo {
        keycode: 9,
        label: "Escape",
        gpui_key: "escape",
    },
    KeycodeInfo {
        keycode: 10,
        label: "1",
        gpui_key: "1",
    },
    KeycodeInfo {
        keycode: 11,
        label: "2",
        gpui_key: "2",
    },
    KeycodeInfo {
        keycode: 12,
        label: "3",
        gpui_key: "3",
    },
    KeycodeInfo {
        keycode: 13,
        label: "4",
        gpui_key: "4",
    },
    KeycodeInfo {
        keycode: 14,
        label: "5",
        gpui_key: "5",
    },
    KeycodeInfo {
        keycode: 15,
        label: "6",
        gpui_key: "6",
    },
    KeycodeInfo {
        keycode: 16,
        label: "7",
        gpui_key: "7",
    },
    KeycodeInfo {
        keycode: 17,
        label: "8",
        gpui_key: "8",
    },
    KeycodeInfo {
        keycode: 18,
        label: "9",
        gpui_key: "9",
    },
    KeycodeInfo {
        keycode: 19,
        label: "0",
        gpui_key: "0",
    },
    KeycodeInfo {
        keycode: 20,
        label: "-",
        gpui_key: "-",
    },
    KeycodeInfo {
        keycode: 21,
        label: "=",
        gpui_key: "=",
    },
    KeycodeInfo {
        keycode: 22,
        label: "Backspace",
        gpui_key: "backspace",
    },
    KeycodeInfo {
        keycode: 23,
        label: "Tab",
        gpui_key: "tab",
    },
    KeycodeInfo {
        keycode: 24,
        label: "Q",
        gpui_key: "q",
    },
    KeycodeInfo {
        keycode: 25,
        label: "W",
        gpui_key: "w",
    },
    KeycodeInfo {
        keycode: 26,
        label: "E",
        gpui_key: "e",
    },
    KeycodeInfo {
        keycode: 27,
        label: "R",
        gpui_key: "r",
    },
    KeycodeInfo {
        keycode: 28,
        label: "T",
        gpui_key: "t",
    },
    KeycodeInfo {
        keycode: 29,
        label: "Y",
        gpui_key: "y",
    },
    KeycodeInfo {
        keycode: 30,
        label: "U",
        gpui_key: "u",
    },
    KeycodeInfo {
        keycode: 31,
        label: "I",
        gpui_key: "i",
    },
    KeycodeInfo {
        keycode: 32,
        label: "O",
        gpui_key: "o",
    },
    KeycodeInfo {
        keycode: 33,
        label: "P",
        gpui_key: "p",
    },
    KeycodeInfo {
        keycode: 34,
        label: "[",
        gpui_key: "[",
    },
    KeycodeInfo {
        keycode: 35,
        label: "]",
        gpui_key: "]",
    },
    KeycodeInfo {
        keycode: 36,
        label: "Enter",
        gpui_key: "enter",
    },
    KeycodeInfo {
        keycode: 38,
        label: "A",
        gpui_key: "a",
    },
    KeycodeInfo {
        keycode: 39,
        label: "S",
        gpui_key: "s",
    },
    KeycodeInfo {
        keycode: 40,
        label: "D",
        gpui_key: "d",
    },
    KeycodeInfo {
        keycode: 41,
        label: "F",
        gpui_key: "f",
    },
    KeycodeInfo {
        keycode: 42,
        label: "G",
        gpui_key: "g",
    },
    KeycodeInfo {
        keycode: 43,
        label: "H",
        gpui_key: "h",
    },
    KeycodeInfo {
        keycode: 44,
        label: "J",
        gpui_key: "j",
    },
    KeycodeInfo {
        keycode: 45,
        label: "K",
        gpui_key: "k",
    },
    KeycodeInfo {
        keycode: 46,
        label: "L",
        gpui_key: "l",
    },
    KeycodeInfo {
        keycode: 47,
        label: ";",
        gpui_key: ";",
    },
    KeycodeInfo {
        keycode: 48,
        label: "'",
        gpui_key: "'",
    },
    KeycodeInfo {
        keycode: 49,
        label: "` (Grave)",
        gpui_key: "`",
    },
    KeycodeInfo {
        keycode: 51,
        label: "\\",
        gpui_key: "\\",
    },
    KeycodeInfo {
        keycode: 52,
        label: "Z",
        gpui_key: "z",
    },
    KeycodeInfo {
        keycode: 53,
        label: "X",
        gpui_key: "x",
    },
    KeycodeInfo {
        keycode: 54,
        label: "C",
        gpui_key: "c",
    },
    KeycodeInfo {
        keycode: 55,
        label: "V",
        gpui_key: "v",
    },
    KeycodeInfo {
        keycode: 56,
        label: "B",
        gpui_key: "b",
    },
    KeycodeInfo {
        keycode: 57,
        label: "N",
        gpui_key: "n",
    },
    KeycodeInfo {
        keycode: 58,
        label: "M",
        gpui_key: "m",
    },
    KeycodeInfo {
        keycode: 59,
        label: ",",
        gpui_key: ",",
    },
    KeycodeInfo {
        keycode: 60,
        label: ".",
        gpui_key: ".",
    },
    KeycodeInfo {
        keycode: 61,
        label: "/",
        gpui_key: "/",
    },
    KeycodeInfo {
        keycode: 65,
        label: "Space",
        gpui_key: "space",
    },
    KeycodeInfo {
        keycode: 67,
        label: "F1",
        gpui_key: "f1",
    },
    KeycodeInfo {
        keycode: 68,
        label: "F2",
        gpui_key: "f2",
    },
    KeycodeInfo {
        keycode: 69,
        label: "F3",
        gpui_key: "f3",
    },
    KeycodeInfo {
        keycode: 70,
        label: "F4",
        gpui_key: "f4",
    },
    KeycodeInfo {
        keycode: 71,
        label: "F5",
        gpui_key: "f5",
    },
    KeycodeInfo {
        keycode: 72,
        label: "F6",
        gpui_key: "f6",
    },
    KeycodeInfo {
        keycode: 73,
        label: "F7",
        gpui_key: "f7",
    },
    KeycodeInfo {
        keycode: 74,
        label: "F8",
        gpui_key: "f8",
    },
    KeycodeInfo {
        keycode: 75,
        label: "F9",
        gpui_key: "f9",
    },
    KeycodeInfo {
        keycode: 76,
        label: "F10",
        gpui_key: "f10",
    },
    KeycodeInfo {
        keycode: 95,
        label: "F11",
        gpui_key: "f11",
    },
    KeycodeInfo {
        keycode: 96,
        label: "F12",
        gpui_key: "f12",
    },
    KeycodeInfo {
        keycode: 107,
        label: "Print Screen",
        gpui_key: "print",
    },
    KeycodeInfo {
        keycode: 110,
        label: "Home",
        gpui_key: "home",
    },
    KeycodeInfo {
        keycode: 111,
        label: "Up",
        gpui_key: "up",
    },
    KeycodeInfo {
        keycode: 112,
        label: "Page Up",
        gpui_key: "pageup",
    },
    KeycodeInfo {
        keycode: 113,
        label: "Left",
        gpui_key: "left",
    },
    KeycodeInfo {
        keycode: 114,
        label: "Right",
        gpui_key: "right",
    },
    KeycodeInfo {
        keycode: 115,
        label: "End",
        gpui_key: "end",
    },
    KeycodeInfo {
        keycode: 116,
        label: "Down",
        gpui_key: "down",
    },
    KeycodeInfo {
        keycode: 117,
        label: "Page Down",
        gpui_key: "pagedown",
    },
    KeycodeInfo {
        keycode: 118,
        label: "Insert",
        gpui_key: "insert",
    },
    KeycodeInfo {
        keycode: 119,
        label: "Delete",
        gpui_key: "delete",
    },
    KeycodeInfo {
        keycode: 127,
        label: "Pause",
        gpui_key: "pause",
    },
    KeycodeInfo {
        keycode: 148,
        label: "XF86Calculator",
        gpui_key: "xf86calculator",
    },
];

#[must_use]
pub fn gpui_key_to_keycode(key: &str) -> Option<u8> {
    KEYCODE_TABLE.iter().find(|e| e.gpui_key == key).map(|e| e.keycode)
}

#[must_use]
pub fn keycode_label(keycode: u8) -> String {
    KEYCODE_TABLE
        .iter()
        .find(|e| e.keycode == keycode)
        .map_or_else(|| format!("Key {keycode}"), |e| e.label.to_string())
}

/// The `preferences.json` a CLI verb should edit in place: the user's file,
/// else the system one when only that exists.
///
/// Resolves the same way [`load_preferences`] reads, so an edit lands in the
/// file the daemon will actually load. Note it is [`platform::config_dir`] —
/// `config_base_dir()` is the *state* root (`~/.local`), and writing
/// preferences there silently has no effect.
#[must_use]
pub fn editable_preferences_path() -> std::path::PathBuf {
    let user = config_dir(&platform::config_dir()).join("preferences.json");
    let system = std::path::PathBuf::from(SYSTEM_PREFERENCES_PATH);
    if !user.exists() && system.exists() {
        system
    } else {
        user
    }
}

#[must_use]
pub fn load_preferences(config_base: &std::path::Path) -> Preferences {
    let user_path = config_dir(config_base).join("preferences.json");
    if let Some(prefs) = read_preferences_file(&user_path) {
        return prefs;
    }
    if let Some(prefs) = read_preferences_file(std::path::Path::new(SYSTEM_PREFERENCES_PATH)) {
        return prefs;
    }
    Preferences::default()
}

fn read_preferences_file(path: &std::path::Path) -> Option<Preferences> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_preferences(config_base: &std::path::Path, prefs: &Preferences) {
    let dir = config_dir(config_base);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("preferences.json");
    if let Ok(data) = serde_json::to_string_pretty(prefs) {
        let _ = std::fs::write(path, data);
    }
}

/// Atomically persist the tab list to `{config_base}/tab-atelier/tabs.json`.
///
/// Rotates `.bak`, `.bak.1`, `.bak.2`; staged via `.tmp` + fsync + rename.
/// Per-tab output should be saved separately with `save_tab_output()` so a
/// bad write to one tab's output cannot corrupt the global tab list.
pub fn save_state(config_base: &std::path::Path, state: &SavedState) {
    let dir = config_dir(config_base);
    let path = dir.join("tabs.json");
    write_atomic_with_rotation(&dir, &path, state, true);
}

/// [`save_state`] for a caller that already holds the pretty-printed JSON.
///
/// The persist ticks serialize the state anyway to CRC-hash it for the
/// dirtiness gate, so re-serializing the identical value inside the
/// writer was duplicate work on every dirty tick.
pub fn save_state_serialized(config_base: &std::path::Path, json: &str) {
    let dir = config_dir(config_base);
    let path = dir.join("tabs.json");
    write_atomic_raw_with_rotation(&dir, &path, json);
}

pub fn save_preferences_at(path: &std::path::Path, prefs: &Preferences) {
    if let Some(parent) = path.parent() {
        write_atomic_with_rotation(parent, path, prefs, true);
    }
    // preferences.json holds plaintext bearer tokens for every
    // configured remote_endpoint. Default umask (0o022 on most
    // distros) would leave the file world-readable; tighten to
    // owner-only the same way `save_api_token` does for api.token.
    // No-op on Windows (mode bits not enforced by NTFS the same
    // way).
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Line cap for the PERIODIC (every-2 s) scrollback saves in both the GUI
/// and headless output savers.
///
/// The full-history walk serializes up to 10 000 lines × cols cells while
/// holding the tab's Term lock — the alacritty parser blocks on that same
/// lock, so a busy tab hiccuped every save tick. 2 000 lines keeps a
/// restart restore deep enough to scroll through while cutting the
/// walk ~5×; the SHUTDOWN flush still saves the full history inline, so a
/// clean quit loses nothing (only a crash/kill restores the capped depth).
pub const PERIODIC_OUTPUT_SAVE_LINES: usize = 2000;

/// Persist a single tab's output buffer to its own file
/// (`{state_base}/tab-atelier/output_tab-<sanitized-name>.json`). Atomic,
/// with one rotated backup.
pub fn save_tab_output(state_base: &std::path::Path, tab_name: &str, output: &str) {
    let dir = state_dir(state_base);
    let path = tab_output_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &output, false);
}

#[must_use]
pub fn load_tab_output(state_base: &std::path::Path, tab_name: &str) -> Option<String> {
    let path = tab_output_path(state_base, tab_name);
    if let Ok(data) = std::fs::read_to_string(&path)
        && let Ok(s) = serde_json::from_str::<String>(&data)
    {
        return Some(s);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(s) = serde_json::from_str::<String>(&data)
    {
        return Some(s);
    }
    None
}

fn write_atomic_with_rotation<T: serde::Serialize>(
    dir: &std::path::Path,
    path: &std::path::Path,
    value: &T,
    pretty: bool,
) {
    let result = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    let Ok(data) = result else {
        return;
    };
    write_atomic_raw_with_rotation(dir, path, &data);
}

/// The write half of [`write_atomic_with_rotation`] for pre-serialized
/// content (see [`save_state_serialized`]).
fn write_atomic_raw_with_rotation(dir: &std::path::Path, path: &std::path::Path, data: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir);

    let tmp = path.with_extension("json.tmp");
    let Ok(mut f) = std::fs::File::create(&tmp) else { return };
    // State files (tabs.json, preferences.json) carry bearer secrets
    // (per-tab share tokens, relay tokens). Restrict to owner-only
    // BEFORE writing the body so the secrets never exist on disk
    // world-readable, even briefly. The final file inherits these
    // perms through the rename, and each `.bak*` rotation is a rename
    // of an already-0600 file, so the backups are protected too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    if f.write_all(data.as_bytes()).is_err() || f.sync_all().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(f);

    if path.exists() {
        let bak = path.with_extension("json.bak");
        let bak1 = path.with_extension("json.bak.1");
        let bak2 = path.with_extension("json.bak.2");
        let _ = std::fs::rename(&bak1, &bak2);
        let _ = std::fs::rename(&bak, &bak1);
        let _ = std::fs::rename(path, &bak);
    }
    let _ = std::fs::rename(&tmp, path);

    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Does a path's last segment look like a `name.ext` filename? Used to
/// promote a single-slash token (`build/poc.php`) to a clickable path
/// while rejecting prose (`and/or`, `TCP/IP`, `24/7`, `2.5`). True when
/// there's a short (≤8) alphanumeric extension after a dot AND the
/// segment contains at least one letter (so pure numbers don't qualify).
#[must_use]
fn looks_like_filename(seg: &str) -> bool {
    let Some((_name, ext)) = seg.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty()
        && ext.len() <= 8
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && seg.chars().any(|c| c.is_ascii_alphabetic())
}

#[must_use]
pub fn detect_urls(text: &str) -> Vec<(usize, usize, String, bool)> {
    // Allocation-free fast path. Every pattern this function detects —
    // `http://`, `https://`, and `/absolute` or `~/relative` paths —
    // contains a `/`. A line with no slash can't match, so bail before
    // the `Vec<char>` allocation + full scan. This runs per cache-
    // missed row in the paint loop; during a number/paste flood
    // (`seq`, a pasted blob) almost every row has no slash, so this
    // turns 50 per-frame allocations into 50 single-byte scans.
    if !text.as_bytes().contains(&b'/') {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut urls = Vec::new();
    let mut i = 0;

    while i < len {
        if chars[i] == 'h' && i + 7 < len {
            let prefix_len = if i + 8 <= len
                && chars[i + 1] == 't'
                && chars[i + 2] == 't'
                && chars[i + 3] == 'p'
                && chars[i + 4] == 's'
                && chars[i + 5] == ':'
                && chars[i + 6] == '/'
                && chars[i + 7] == '/'
            {
                8
            } else if i + 7 <= len
                && chars[i + 1] == 't'
                && chars[i + 2] == 't'
                && chars[i + 3] == 'p'
                && chars[i + 4] == ':'
                && chars[i + 5] == '/'
                && chars[i + 6] == '/'
            {
                7
            } else {
                0
            };
            if prefix_len > 0 {
                let start = i;
                while i < len
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '"' | '\'' | '<' | '>' | ')' | ']' | '}')
                {
                    i += 1;
                }
                // Trailing punctuation that's almost never part of the
                // URL itself — sentence terminators (`.` `,` `;`) and
                // the line/byte-offset separator (`:`) that compilers,
                // grep, tracebacks etc. append to a path or URL
                // (`https://example.com/x:` from a log message, or
                // `/mnt/foo.pdf:` from an `ls -la` style line).
                while i > start + prefix_len && matches!(chars[i - 1], '.' | ',' | ';' | ':') {
                    i -= 1;
                }
                let url: String = chars[start..i].iter().collect();
                urls.push((start, i, url, false));
                continue;
            }
        }

        if chars[i] == '/' && i + 1 < len && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '.') {
            let mut start = i;
            while start > 0 && (chars[start - 1].is_alphanumeric() || matches!(chars[start - 1], '_' | '-' | '.')) {
                start -= 1;
            }
            // Pick up a leading `~` so home-relative paths like
            // `~/.local/state/tab-atelier/tabs.json` are detected as a whole.
            // Same for `$VAR/...` style env-var prefixes.
            if start > 0 && matches!(chars[start - 1], '~' | '$') {
                start -= 1;
            }
            // A path never begins with a flag dash. Tools glue a flag straight
            // onto a relative/absolute path with no separator — dpkg prints
            // `-O../foo`, gcc `-I../inc`, `-o./out`. When the backward scan
            // stopped on a `-`, that `-` starts a flag, not the path: re-anchor
            // to the first real path start (`/`, `./`, or `../`) inside the token
            // so the flag isn't linkified. (A `--opt=../x` form already stops at
            // the `=`, which isn't a path char.)
            if chars[start] == '-' {
                let mut p = start + 1;
                while p < i {
                    let rel_marker = chars[p] == '.'
                        && (chars.get(p + 1) == Some(&'/')
                            || (chars.get(p + 1) == Some(&'.') && chars.get(p + 2) == Some(&'/')));
                    if chars[p] == '/' || rel_marker {
                        break;
                    }
                    p += 1;
                }
                start = p;
            }
            let mut j = i;
            while j < len
                && !chars[j].is_whitespace()
                && !matches!(chars[j], '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '|' | '│')
            {
                j += 1;
            }
            // Same trailing-punctuation strip as the URL branch above.
            // `:` covers grep / compiler / traceback suffixes
            // (`/mnt/Dev/questionnaire.pdf:` in `ls -la`-style output).
            while j > start + 1 && matches!(chars[j - 1], '.' | ',' | ';' | ':') {
                j -= 1;
            }
            let path: String = chars[start..j].iter().collect();
            // ≥2 slashes ⇒ unambiguous path (`/a/b`, `src/x/y`). A
            // SINGLE-slash token is a path only when its last segment
            // looks like a filename (`build/poc.php`, `src/main.rs`) —
            // that filter rejects prose like `and/or`, `TCP/IP`, `24/7`
            // while catching relative file paths a tool just printed.
            let slashes = path.matches('/').count();
            let single_slash_file = slashes == 1 && looks_like_filename(path.rsplit('/').next().unwrap_or(""));
            // A leading `./` or `../` is an unambiguous relative-path marker —
            // accept it regardless of the filename heuristic, so a long or
            // unusual extension (`../foo.buildinfo`, ext > 8 chars) still links.
            let relative_marker = path.starts_with("./") || path.starts_with("../");
            if slashes >= 2 || single_slash_file || relative_marker {
                urls.push((start, j, path, true));
                i = j;
                continue;
            }
        }

        if i + 4 < len && chars[i].is_alphanumeric() {
            let start = i;
            let mut j = i;
            while j < len && !chars[j].is_whitespace() && !matches!(chars[j], '"' | '\'' | '<' | '>' | ')' | ']' | '}')
            {
                j += 1;
            }
            // Same trailing-punctuation strip as the URL branch above.
            // `:` covers grep / compiler / traceback suffixes
            // (`/mnt/Dev/questionnaire.pdf:` in `ls -la`-style output).
            while j > start + 1 && matches!(chars[j - 1], '.' | ',' | ';' | ':') {
                j -= 1;
            }
            let candidate: String = chars[start..j].iter().collect();
            if candidate.contains('/') && candidate.contains(':') {
                let has_slash = candidate.matches('/').count() >= 1;
                let colon_part = candidate.rsplit(':').next().unwrap_or("");
                let looks_like_path =
                    has_slash && !colon_part.is_empty() && colon_part.chars().all(|c| c.is_ascii_digit());
                if looks_like_path && !candidate.starts_with("http") {
                    urls.push((start, j, candidate, true));
                    i = j;
                    continue;
                }
            }
        }

        i += 1;
    }

    urls
}

/// The `<name> vX.Y.Z (<build-hash>)` line both binaries print for `--version`.
///
/// `BUILD_HASH` is `git rev-parse --short=12 HEAD` at compile time (see
/// `build.rs`), so it pins the exact commit a deployed binary was built from —
/// which the crate version alone can't answer (every nightly is `0.5.0-dev`),
/// making "is this binary actually up to date?" a one-command check.
#[must_use]
pub fn version_line(bin: &str) -> String {
    format!("{bin} v{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_HASH"))
}

/// Strip ANSI CSI/SGR escapes (`ESC [ … final`) from `s`.
///
/// Used when copying scrollback to the system clipboard so the receiving
/// app doesn't see raw escape sequences. Persistence and the mobile API
/// endpoints keep colours intentionally and bypass this helper.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                // CSI parameters are `0x30..=0x3F`, intermediates `0x20..=0x2F`,
                // and the sequence ends at the first byte in `0x40..=0x7E`.
                if ('\x40'..='\x7e').contains(&nc) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[must_use]
pub fn file_path_for_open(path: &str) -> &str {
    if let Some(colon_pos) = path.rfind(':') {
        let after = &path[colon_pos + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            let base = &path[..colon_pos];
            if let Some(colon_pos2) = base.rfind(':') {
                let after2 = &base[colon_pos2 + 1..];
                if !after2.is_empty() && after2.chars().all(|c| c.is_ascii_digit()) {
                    return &path[..colon_pos2];
                }
            }
            return base;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_carries_name_version_and_nonempty_build_hash() {
        let v = version_line("tab-atelier");
        assert!(v.starts_with("tab-atelier v"), "got: {v}");
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
        // Build hash is present, non-empty, and parenthesised at the end.
        assert!(
            !env!("BUILD_HASH").is_empty(),
            "build.rs must set a non-empty BUILD_HASH"
        );
        assert!(v.ends_with(')') && v.contains(" ("), "hash must be in parens: {v}");
    }

    #[test]
    fn agent_launch_suffix_execs_the_resume_command() {
        let s = agent_launch_shell_suffix("claude", "abc-123", None).unwrap();
        // Interactive shell (sources rc for PATH), clear the grid, then exec the
        // agent so a prior run's tail can't linger below the fresh UI.
        assert_eq!(
            s,
            vec![
                "-i",
                "-c",
                r"printf '\033[3J\033[H\033[2J'; exec claude --resume abc-123"
            ]
        );
        assert!(s.last().unwrap().starts_with(AGENT_LAUNCH_CLEAR));
        // catbus + plan flag flows through build_agent_resume_command.
        let c = agent_launch_shell_suffix("catbus", "s1", Some(true)).unwrap();
        assert_eq!(
            c.last().unwrap(),
            r"printf '\033[3J\033[H\033[2J'; exec catbus-agent --resume s1 --plan"
        );
        // Unknown kind → no direct launch (caller keeps the plain shell).
        assert!(agent_launch_shell_suffix("bash", "x", None).is_none());
    }

    #[test]
    fn fresh_claude_suffix_execs_claude_in_auto_mode() {
        // Claude-only mode launches a fresh `claude` in `auto` mode (the
        // ⏵⏵ auto mode footer) — no --resume, since there's no session yet.
        assert_eq!(FRESH_CLAUDE_AUTO_CMD, "claude --permission-mode auto");
        let s = fresh_claude_launch_suffix();
        assert_eq!(
            s,
            vec![
                "-i",
                "-c",
                r"printf '\033[3J\033[H\033[2J'; exec claude --permission-mode auto",
            ]
        );
        // Same clear-then-exec shape as the resume path.
        assert!(s.last().unwrap().starts_with(AGENT_LAUNCH_CLEAR));
        assert!(s.last().unwrap().contains(FRESH_CLAUDE_AUTO_CMD));
    }

    #[test]
    fn instrumented_suffix_clears_the_grid_before_exec() {
        // The instrumented variant is the path the GUI/headless actually use.
        // Regardless of whether tracing/frame-timing env is on (which prefixes
        // strace / env vars onto the exec), the command must start with the grid
        // clear so a prior agent run's tail can't linger under the fresh UI, then
        // exec the resumed agent.
        let s = agent_launch_shell_suffix_instrumented("claude", "abc-123", None, None).unwrap();
        assert_eq!(&s[..2], ["-i", "-c"]);
        let cmd = s.last().unwrap();
        assert!(cmd.starts_with(AGENT_LAUNCH_CLEAR), "must clear first: {cmd}");
        assert!(cmd.contains("exec"), "then exec: {cmd}");
        assert!(cmd.contains("claude --resume abc-123"), "the resumed agent: {cmd}");
        // Unknown kind → no direct launch, same as the plain variant.
        assert!(agent_launch_shell_suffix_instrumented("bash", "x", None, None).is_none());
    }

    #[test]
    fn brain_resumes_by_relaunch_ignoring_session() {
        // The brain watchdog has no session; auto-resume just relaunches it.
        // `session_id` is ignored, so an empty one still yields the command.
        let cmd = build_agent_resume_command("brain", "", None).unwrap();
        #[cfg(feature = "gui")]
        assert_eq!(cmd, "tab-atelier brain");
        #[cfg(not(feature = "gui"))]
        assert_eq!(cmd, "tab-atelier-headless brain");
    }

    #[test]
    fn a_respawned_agent_tab_is_always_relaunched_exactly_one_way() {
        use AgentRelaunch::{Exec, None as NoRelaunch, Typed};
        // The bug this encodes: a colours / net toggle re-forked the shell and
        // did NEITHER, so an agent tab came back as a bare shell.
        assert_eq!(agent_relaunch_mode(true, true, false), Exec);
        assert_eq!(agent_relaunch_mode(true, false, false), Typed);
        // Doing both would double-launch the session, so the two are exclusive
        // by construction — one enum, not two independent flags.
        assert_ne!(
            agent_relaunch_mode(true, true, false),
            agent_relaunch_mode(true, false, false)
        );
        // A plain shell tab stays a plain shell.
        assert_eq!(agent_relaunch_mode(false, true, false), NoRelaunch);
        assert_eq!(agent_relaunch_mode(false, false, false), NoRelaunch);
        // Read-only never resumes, whatever the env mode.
        assert_eq!(agent_relaunch_mode(true, true, true), NoRelaunch);
        assert_eq!(agent_relaunch_mode(true, false, true), NoRelaunch);
    }

    #[test]
    fn a_flagged_daemon_relaunches_as_our_own_subcommand() {
        // A harness registers its watcher with `set-status --kind <verb>
        // --daemon`; restore relaunches it the way it relaunches brain, with
        // no match arm per creature.
        assert_eq!(
            daemon_relaunch_command("aligator").unwrap(),
            format!("{} aligator", cli_binary_name())
        );
        // The FLAG elects a daemon — an unrecognised kind on its own still
        // restores to a plain shell, so a stray `set-status --kind foo` can
        // never become a command line at restore.
        assert!(build_agent_resume_command("aligator", "", None).is_none());
        assert!(build_agent_resume_command("bash", "x", None).is_none());
        // Session agents keep their own resume shape, and are never daemons.
        assert_eq!(
            build_agent_resume_command("claude", "sess-1", None).unwrap(),
            "claude --resume sess-1"
        );
        assert!(!is_daemon_kind("claude") && !is_daemon_kind("catbus"));
        // Even flagged, anything that isn't a plain lowercase verb is refused:
        // the relaunch is always OUR binary plus one subcommand, leaving no
        // room for an argument, a separator or a substitution.
        for bad in ["rm -rf /", "brain;reboot", "Brain", "$(id)", "b", &"x".repeat(25)] {
            assert!(!is_daemon_kind(bad), "{bad} must not be a daemon kind");
            assert!(daemon_relaunch_command(bad).is_none());
        }
    }

    #[test]
    fn the_stats_block_has_the_same_rows_whatever_the_data_says() {
        // The bug this encodes, twice over: rows used to be pushed only when a
        // value existed, so the menu grew as the sampler answered — and since
        // it can open upward, the item under the cursor changed mid-click.
        let labels = ["CPU", "Power", "Memory", "Connections"];
        let full: Vec<(&str, Option<String>)> = labels.iter().map(|l| (*l, Some("12".to_string()))).collect();
        let empty: Vec<(&str, Option<String>)> = labels.iter().map(|l| (*l, None)).collect();
        let mixed: Vec<(&str, Option<String>)> = vec![
            ("CPU", Some("3.4%".into())),
            ("Power", None),
            ("Memory", Some("41 MB".into())),
            ("Connections", None),
        ];
        assert_eq!(stats_rows(&full).len(), labels.len());
        assert_eq!(stats_rows(&empty).len(), labels.len(), "no data is still every row");
        assert_eq!(stats_rows(&mixed).len(), labels.len());
        // Labels keep their order and their position, so the Nth row is always
        // the same reading.
        assert_eq!(stats_rows(&mixed)[0], "CPU: 3.4%");
        assert_eq!(stats_rows(&mixed)[1], format!("Power: {STAT_PENDING}"));
        assert_eq!(stats_rows(&mixed)[3], format!("Connections: {STAT_PENDING}"));
        assert!(stats_rows(&[]).is_empty());
    }

    #[test]
    fn a_peer_presents_its_relay_token_and_falls_back_to_the_master() {
        // One endpoint entry feeds two consumers with different rights: the
        // sidecar needs the peer's master token (it lists tabs, types input,
        // moves files), the relay hop must present the relay-only one. Mixing
        // them up means either the sidecar 401s or the relay does.
        let mut ep = RemoteEndpoint {
            id: "id".into(),
            label: "box".into(),
            url: "https://box:7891".into(),
            token: "MASTER".into(),
            ..RemoteEndpoint::default()
        };
        assert_eq!(
            relay_credential(&ep),
            "MASTER",
            "no relay token yet → the old behaviour"
        );
        ep.relay_token = "RELAY".into();
        assert_eq!(relay_credential(&ep), "RELAY", "the relay hop uses the relay token");
        assert_eq!(ep.token, "MASTER", "and the sidecar's credential is untouched");
        // Whitespace-only is not a credential.
        ep.relay_token = String::new();
        assert_eq!(relay_credential(&ep), "MASTER");
    }

    #[test]
    fn the_relay_stand_in_key_is_never_the_master_token() {
        // Relay mode puts a value in ANTHROPIC_API_KEY, which tools copy into
        // debug output and transcripts far more readily than a deliberate
        // credential. It must not be the token that administers every tab.
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("KEEP".to_string(), "1".to_string());
        let env = tab_env_extras("tab-1", "http://127.0.0.1:7890", "MASTER-TOKEN", &extra);
        // The API token is still exported — that's the deliberate one, used by
        // set-status and the teamwork verbs from inside the tab.
        assert_eq!(
            env.get("TAB_ATELIER_API_TOKEN").map(String::as_str),
            Some("MASTER-TOKEN")
        );
        if let Some(key) = env.get("ANTHROPIC_API_KEY") {
            assert_ne!(key, "MASTER-TOKEN", "the stand-in key must not be the master token");
            assert_eq!(key, &relay_token());
        }
        // The relay token is stable across calls, so a peer configured with it
        // keeps working; and it is not the master.
        assert_eq!(relay_token(), relay_token());
        assert!(!relay_token().is_empty());
        assert_ne!(relay_token(), "MASTER-TOKEN");
    }

    #[test]
    fn a_pending_stat_still_occupies_its_row() {
        // The row has to be there before the sampler answers, or the menu
        // changes height while it is open and the click lands on the wrong
        // item. An empty string counts as "not yet" for the same reason —
        // `watts_label` returns "" when RAPL has no reading.
        assert_eq!(stat_value(Some("12.5%".into())), "12.5%");
        assert_eq!(stat_value(None), STAT_PENDING);
        assert_eq!(stat_value(Some(String::new())), STAT_PENDING);
        assert_eq!(stat_value(Some("   ".into())), STAT_PENDING);
        assert!(!STAT_PENDING.is_empty(), "the placeholder must render as a row");
    }

    #[test]
    fn escape_digs_out_of_one_layer_at_a_time() {
        use Overlay::{ContextMenu, Preferences, Renaming, TabSwitcher};
        // Nothing open: Escape belongs to the tab. Swallowing it here would
        // break vim's insert mode in every tab.
        assert_eq!(escape_dismisses(OverlayState::default()), None);
        // The case that prompted this: a stray double-click on a tab starts a
        // rename, which gates BOTH Ctrl+P and the context menu — so the whole
        // window looks dead with no visible cause. Escape must get out of it.
        let renaming = OverlayState {
            renaming: true,
            ..OverlayState::default()
        };
        assert_eq!(escape_dismisses(renaming), Some(Renaming));
        // One press, one layer: the menu goes before what it sits on.
        let stacked = OverlayState {
            context_menu: true,
            renaming: true,
            preferences: true,
            ..OverlayState::default()
        };
        assert_eq!(escape_dismisses(stacked), Some(ContextMenu));
        let after_menu = OverlayState {
            context_menu: false,
            ..stacked
        };
        assert_eq!(escape_dismisses(after_menu), Some(Renaming));
        assert_eq!(
            escape_dismisses(OverlayState {
                renaming: false,
                ..after_menu
            }),
            Some(Preferences)
        );
        // Every layer is reachable on its own, so none can strand the window.
        for (state, want) in [
            (
                OverlayState {
                    tab_switcher: true,
                    ..OverlayState::default()
                },
                TabSwitcher,
            ),
            (
                OverlayState {
                    hotkey_picker: true,
                    ..OverlayState::default()
                },
                Overlay::HotkeyPicker,
            ),
            (
                OverlayState {
                    qr: true,
                    ..OverlayState::default()
                },
                Overlay::Qr,
            ),
            (
                OverlayState {
                    close_confirm: true,
                    ..OverlayState::default()
                },
                Overlay::CloseConfirm,
            ),
            (
                OverlayState {
                    exit_confirm: true,
                    ..OverlayState::default()
                },
                Overlay::ExitConfirm,
            ),
            (
                OverlayState {
                    preferences: true,
                    ..OverlayState::default()
                },
                Preferences,
            ),
        ] {
            assert_eq!(escape_dismisses(state), Some(want));
        }
    }

    #[test]
    fn app_chords_survive_capslock_and_stay_off_the_ptys_keys() {
        use AppChord::{Copy, NewTab, NextTab, Paste, TabSwitcher};
        // The regression: an exact "p" comparison drops the chord when the
        // keysym arrives uppercase (CapsLock, or a layout's shifted level),
        // and Ctrl+P silently stops opening the switcher.
        assert_eq!(app_chord("p", true, false, false), Some(TabSwitcher));
        assert_eq!(app_chord("P", true, false, false), Some(TabSwitcher));
        assert_eq!(app_chord("t", true, true, false), Some(NewTab));
        assert_eq!(app_chord("T", true, true, false), Some(NewTab));
        assert_eq!(app_chord("c", true, true, false), Some(Copy));
        assert_eq!(app_chord("v", true, true, false), Some(Paste));
        assert_eq!(app_chord("tab", false, false, true), Some(NextTab));
        assert_eq!(
            app_chord("TAB", false, true, true),
            Some(NextTab),
            "shift is ignored here"
        );

        // Everything else belongs to the PTY. In particular a bare letter and
        // the wrong modifier set must reach the shell untouched — swallowing
        // them here would make the key do nothing at all.
        assert_eq!(app_chord("p", false, false, false), None);
        assert_eq!(
            app_chord("p", true, true, false),
            None,
            "Ctrl+Shift+P is not the switcher"
        );
        assert_eq!(
            app_chord("p", true, false, true),
            None,
            "Ctrl+Alt+P is not the switcher"
        );
        assert_eq!(app_chord("t", true, false, false), None, "Ctrl+T is the shell's");
        assert_eq!(
            app_chord("c", true, false, false),
            None,
            "Ctrl+C must interrupt, not copy"
        );
        assert_eq!(app_chord("v", true, false, false), None);
        assert_eq!(app_chord("tab", false, false, false), None, "plain Tab completes");
        assert_eq!(app_chord("tab", true, false, false), None);
        assert_eq!(app_chord("", true, false, false), None);
    }

    #[test]
    fn folder_styles_can_be_replaced_while_running() {
        // The bug this encodes: the rules lived in a OnceLock, so editing a
        // rule silently no-op'd until the app was restarted — with dozens of
        // tabs open, nobody restarts to try a colour.
        let rule = |color: &str| {
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "/tmp/ta-style-test".to_string(),
                FolderStyle {
                    color: Some(color.to_string()),
                    badge: Some("T".into()),
                },
            );
            m
        };
        set_folder_styles(rule("#111111"));
        assert_eq!(
            folder_style_of(Some("/tmp/ta-style-test/sub")).color.as_deref(),
            Some("#111111")
        );
        set_folder_styles(rule("#222222"));
        assert_eq!(
            folder_style_of(Some("/tmp/ta-style-test/sub")).color.as_deref(),
            Some("#222222"),
            "a re-set must win — a OnceLock would have kept the first"
        );
        // An unruled path resolves to the empty style, not the last rule.
        assert_eq!(folder_style_of(Some("/tmp/elsewhere")), FolderStyle::default());
        set_folder_styles(std::collections::BTreeMap::new());
    }

    #[test]
    fn folder_rules_resolve_by_longest_match() {
        let mut styles = std::collections::BTreeMap::new();
        styles.insert(
            "/home/w/Dev/app".to_string(),
            FolderStyle {
                color: Some("#111111".into()),
                badge: Some("APP".into()),
            },
        );
        styles.insert(
            "/home/w/Dev/app/frontend".to_string(),
            FolderStyle {
                color: Some("#222222".into()),
                badge: None,
            },
        );
        let style_of = |cwd| folder_style_for(&styles, Some(cwd));
        // The rule applies to the folder itself and everything under it.
        assert_eq!(style_of("/home/w/Dev/app").unwrap().color.as_deref(), Some("#111111"));
        assert_eq!(
            style_of("/home/w/Dev/app/src/deep").unwrap().color.as_deref(),
            Some("#111111")
        );
        // A sub-project refines its parent instead of fighting it.
        assert_eq!(
            style_of("/home/w/Dev/app/frontend/src").unwrap().color.as_deref(),
            Some("#222222")
        );
        // Prefix match must respect path components: `app-legacy` is NOT `app`.
        assert!(style_of("/home/w/Dev/app-legacy").is_none());
        assert!(style_of("/home/w/Dev").is_none());
        assert!(folder_style_for(&styles, None).is_none());
    }

    #[test]
    fn tab_style_resolves_per_tab_then_folder_then_global() {
        // Background always resolves to something paintable …
        assert_eq!(
            effective_tab_bg(Some("#aaa111"), Some("#bbb222"), Some("#ccc333")),
            "#aaa111"
        );
        assert_eq!(effective_tab_bg(None, Some("#bbb222"), Some("#ccc333")), "#bbb222");
        assert_eq!(effective_tab_bg(None, None, Some("#ccc333")), "#ccc333");
        assert_eq!(effective_tab_bg(None, None, None), DEFAULT_TAB_BG_COLOR);
        // … while the desktop tint stays None until something styles the tab,
        // so an unstyled tab keeps the theme's background.
        assert_eq!(effective_tab_tint(None, Some("#bbb222")), Some("#bbb222"));
        assert_eq!(effective_tab_tint(Some("#aaa111"), Some("#bbb222")), Some("#aaa111"));
        assert_eq!(effective_tab_tint(None, None), None);
        assert_eq!(effective_tab_badge(None, Some("APP")), Some("APP"));
        assert_eq!(effective_tab_badge(Some("ME"), Some("APP")), Some("ME"));
        assert_eq!(effective_tab_badge(None, None), None);
    }

    #[test]
    fn badges_and_colors_are_validated() {
        assert_eq!(sanitize_badge("  KAL "), Ok("KAL".to_string()));
        assert_eq!(sanitize_badge("a\r\nb"), Ok("ab".to_string()));
        assert!(sanitize_badge("   ").is_err());
        assert!(sanitize_badge(&"x".repeat(BADGE_MAX + 1)).is_err());
        assert!(sanitize_badge("🐊").is_ok(), "an emoji is one char");
        assert_eq!(parse_hex_rgb("#7a1f2b"), Some(0x7a_1f_2b));
        assert_eq!(parse_hex_rgb("#FFFFFF"), Some(0xff_ff_ff));
        for bad in ["7a1f2b", "#7a1f2", "#7a1f2bb", "#gggggg", "", "#"] {
            assert!(parse_hex_rgb(bad).is_none(), "{bad} must not parse");
        }
    }

    #[test]
    fn meta_keys_are_normalised_and_bounded() {
        assert_eq!(
            sanitize_meta(" Role ", "  reviewer  ").unwrap(),
            ("role".to_string(), "reviewer".to_string())
        );
        // Control characters would corrupt a header line or a log — stripped.
        assert_eq!(sanitize_meta("k", "a\r\nb").unwrap().1, "ab");
        assert!(sanitize_meta("", "v").is_err());
        assert!(sanitize_meta("has space", "v").is_err());
        assert!(sanitize_meta("k", "   ").is_err(), "empty value → use --clear");
        assert!(sanitize_meta(&"k".repeat(META_KEY_MAX + 1), "v").is_err());
        assert!(sanitize_meta("k", &"v".repeat(META_VALUE_MAX + 1)).is_err());
        assert!(sanitize_meta("k", &"é".repeat(META_VALUE_MAX)).is_ok(), "cap is chars");
    }

    #[test]
    fn meta_map_stays_bounded_whatever_the_api_allowed() {
        let mut map = std::collections::BTreeMap::new();
        for i in 0..META_MAX_KEYS {
            apply_meta_change(&mut map, &format!("k{i}"), Some("v".into()));
        }
        assert_eq!(map.len(), META_MAX_KEYS);
        // A new key on a full map is dropped, so tabs.json can't grow without
        // bound even if a racing writer slipped past the API's check.
        apply_meta_change(&mut map, "overflow", Some("v".into()));
        assert_eq!(map.len(), META_MAX_KEYS);
        assert!(!map.contains_key("overflow"));
        // Updating an existing key always works, and None removes.
        apply_meta_change(&mut map, "k0", Some("v2".into()));
        assert_eq!(map.get("k0").map(String::as_str), Some("v2"));
        apply_meta_change(&mut map, "k0", None);
        assert_eq!(map.len(), META_MAX_KEYS - 1);
    }

    #[test]
    fn wrap_exec_command_prefixes_tracer_when_present() {
        // No tracer, no frames, no title → the plain suffix's exec line.
        assert_eq!(
            wrap_exec_command("claude --resume x", None, None, None),
            "exec claude --resume x"
        );
        // With a tracer → strace counts syscalls, output to the quoted log.
        let trace = ("/usr/bin/strace".to_string(), "/var/lib/tab-atelier/t.txt".to_string());
        assert_eq!(
            wrap_exec_command("claude --resume x", Some(&trace), None, None),
            "exec '/usr/bin/strace' -f -c -o '/var/lib/tab-atelier/t.txt' claude --resume x"
        );
    }

    #[test]
    fn wrap_exec_command_sets_proctitle_via_exec_a() {
        // proctitle → argv[0] override so the process shows the tab name.
        assert_eq!(
            wrap_exec_command("claude --resume x", None, None, Some("my tab")),
            "exec -a 'my tab' claude --resume x"
        );
        // Combined with the tracer, the title names the outermost program.
        let trace = ("/usr/bin/strace".to_string(), "/s/t.txt".to_string());
        assert_eq!(
            wrap_exec_command("claude --resume x", Some(&trace), None, Some("oa3")),
            "exec -a 'oa3' '/usr/bin/strace' -f -c -o '/s/t.txt' claude --resume x"
        );
    }

    #[test]
    fn wrap_exec_command_prefixes_frame_timing_env() {
        // frames → env-var prefix on `exec`; `exec -a <title>` still renames
        // the agent (not an `env` wrapper), and the quoted log path survives.
        assert_eq!(
            wrap_exec_command("claude --resume x", None, Some("/s/frames.jsonl"), None),
            "CLAUDE_CODE_FRAME_TIMING_LOG='/s/frames.jsonl' CLAUDE_CODE_DEBUG_REPAINTS=1 exec claude --resume x"
        );
        assert_eq!(
            wrap_exec_command("claude --resume x", None, Some("/s/f.jsonl"), Some("t2")),
            "CLAUDE_CODE_FRAME_TIMING_LOG='/s/f.jsonl' CLAUDE_CODE_DEBUG_REPAINTS=1 exec -a 't2' claude --resume x"
        );
        // Tracer + frames compose: env prefix, then exec, then strace, then agent.
        let trace = ("/usr/bin/strace".to_string(), "/s/t.txt".to_string());
        assert_eq!(
            wrap_exec_command("claude --resume x", Some(&trace), Some("/s/f.jsonl"), Some("oa3")),
            "CLAUDE_CODE_FRAME_TIMING_LOG='/s/f.jsonl' CLAUDE_CODE_DEBUG_REPAINTS=1 exec -a 'oa3' '/usr/bin/strace' -f -c -o '/s/t.txt' claude --resume x"
        );
    }

    #[test]
    fn shell_exec_a_support() {
        assert!(shell_supports_exec_a("/bin/bash"));
        assert!(shell_supports_exec_a("/usr/bin/zsh"));
        assert!(!shell_supports_exec_a("/bin/dash"));
        assert!(!shell_supports_exec_a("/usr/bin/fish"));
        assert!(!shell_supports_exec_a("/bin/sh"));
    }

    #[test]
    fn no_internet_command_wraps_in_bwrap_unshare_net() {
        let (prog, args) = no_internet_command("/bin/bash", &["-l".to_string()]);
        assert_eq!(prog, "bwrap");
        // The airgap flag + the real command after the `--` separator.
        assert!(
            args.contains(&"--unshare-net".to_string()),
            "isolates the network namespace"
        );
        assert!(args.contains(&"--die-with-parent".to_string()));
        let sep = args.iter().position(|a| a == "--").expect("has -- separator");
        assert_eq!(
            &args[sep + 1..],
            &["/bin/bash".to_string(), "-l".to_string()],
            "real cmd after --"
        );
    }

    #[test]
    fn drop_caps_command_strips_ambient_and_blocks_regain() {
        let (prog, args) = drop_caps_command("/bin/bash", &["-l".to_string()]);
        assert_eq!(prog, "setpriv");
        // Ambient is the set that would carry CAP_NET_ADMIN into the tab.
        assert!(args.contains(&"--ambient-caps=-all".to_string()), "clears ambient caps");
        assert!(args.contains(&"--no-new-privs".to_string()), "blocks regaining privs");
        // Bounding-set drop is deliberately NOT used (needs CAP_SETPCAP).
        assert!(
            !args.iter().any(|a| a.starts_with("--bounding-set")),
            "no bounding-set drop"
        );
        let sep = args.iter().position(|a| a == "--").expect("has -- separator");
        assert_eq!(
            &args[sep + 1..],
            &["/bin/bash".to_string(), "-l".to_string()],
            "real cmd after --"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cap_amb_nonzero_gates_the_cap_strip() {
        // The regression: setpriv was applied even with no ambient caps,
        // breaking tabs. We only strip when CapAmb is non-zero.
        assert!(!cap_amb_nonzero("Name:\tx\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1"));
        assert!(cap_amb_nonzero("CapAmb:\t0000000000001000")); // CAP_NET_ADMIN
        assert!(!cap_amb_nonzero("no cap line here"));
        assert!(!cap_amb_nonzero("CapAmb:\tnothex"));
    }

    #[test]
    fn parse_memory_bytes_handles_suffixes_and_junk() {
        assert_eq!(parse_memory_bytes("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("1k"), Some(1024));
        assert_eq!(parse_memory_bytes("1048576"), Some(1_048_576));
        assert_eq!(parse_memory_bytes("  4M "), Some(4 * 1024 * 1024));
        assert_eq!(parse_memory_bytes(""), None);
        assert_eq!(parse_memory_bytes("abc"), None);
        assert_eq!(parse_memory_bytes("12X"), None);
    }

    #[test]
    fn parse_meminfo_total_reads_kb_line_as_bytes() {
        let sample = "MemTotal:       65780904 kB\nMemFree:  1234 kB\nMemAvailable: 5678 kB\n";
        assert_eq!(parse_meminfo_total(sample), Some(65_780_904 * 1024));
        // First matching line wins; leading fields are tolerated.
        assert_eq!(
            parse_meminfo_total("MemFree: 10 kB\nMemTotal: 1024 kB"),
            Some(1024 * 1024)
        );
        // Missing / malformed → None.
        assert_eq!(parse_meminfo_total("MemFree: 10 kB"), None);
        assert_eq!(parse_meminfo_total("MemTotal: notanumber kB"), None);
        assert_eq!(parse_meminfo_total(""), None);
    }

    #[test]
    fn tab_limits_resolve_per_tab_over_global() {
        let global = TabResourceLimits {
            memory_max: Some("1G".into()),
            cpu_quota_percent: Some(100),
            tasks_max: Some(512),
        };
        let per_tab = TabResourceLimits {
            memory_max: Some("256M".into()),
            cpu_quota_percent: None,
            tasks_max: None,
        };
        let eff = TabResourceLimits::resolve(&per_tab, &global);
        assert_eq!(eff.memory_max.as_deref(), Some("256M"), "per-tab memory wins");
        assert_eq!(eff.cpu_quota_percent, Some(100), "cpu falls back to global");
        assert_eq!(eff.tasks_max, Some(512), "tasks falls back to global");
        assert_eq!(eff.memory_max_bytes(), Some(256 * 1024 * 1024));
    }

    #[test]
    fn tab_limits_cpu_max_line_and_emptiness() {
        let half = TabResourceLimits {
            cpu_quota_percent: Some(50),
            ..Default::default()
        };
        assert_eq!(half.cpu_max_line().as_deref(), Some("50000 100000"), "half a core");
        let multi = TabResourceLimits {
            cpu_quota_percent: Some(250),
            ..Default::default()
        };
        assert_eq!(multi.cpu_max_line().as_deref(), Some("250000 100000"), "2.5 cores");
        assert!(TabResourceLimits::default().is_empty());
        assert!(
            !TabResourceLimits {
                tasks_max: Some(10),
                ..Default::default()
            }
            .is_empty()
        );
        // Zero percent = no CPU cap line (avoids writing a 0-quota
        // cgroup that would freeze the tab).
        assert!(
            TabResourceLimits {
                cpu_quota_percent: Some(0),
                ..Default::default()
            }
            .cpu_max_line()
            .is_none()
        );
    }

    #[test]
    fn tab_limits_merge_overrides_only_some_axes() {
        let mut base = TabResourceLimits {
            memory_max: Some("1G".into()),
            cpu_quota_percent: Some(100),
            tasks_max: Some(512),
        };
        // A partial override touches only the axes it sets.
        base.merge(&TabResourceLimits {
            cpu_quota_percent: Some(250),
            ..Default::default()
        });
        assert_eq!(base.memory_max.as_deref(), Some("1G"), "memory untouched");
        assert_eq!(base.cpu_quota_percent, Some(250), "cpu replaced");
        assert_eq!(base.tasks_max, Some(512), "tasks untouched");
        // Overriding memory replaces just that axis.
        base.merge(&TabResourceLimits {
            memory_max: Some("2G".into()),
            ..Default::default()
        });
        assert_eq!(base.memory_max.as_deref(), Some("2G"));
        assert_eq!(base.cpu_quota_percent, Some(250), "cpu still from prior merge");
        // An empty override is a no-op.
        let before = base.clone();
        base.merge(&TabResourceLimits::default());
        assert_eq!(base, before, "empty override changes nothing");
    }

    #[test]
    fn tab_limits_memory_max_valid_gates_bad_values() {
        // Unset is valid (nothing to reject).
        assert!(TabResourceLimits::default().memory_max_valid());
        // Parseable values pass.
        assert!(
            TabResourceLimits {
                memory_max: Some("8G".into()),
                ..Default::default()
            }
            .memory_max_valid()
        );
        // Garbage is rejected up front.
        assert!(
            !TabResourceLimits {
                memory_max: Some("lots".into()),
                ..Default::default()
            }
            .memory_max_valid()
        );
    }

    #[test]
    fn minimal_pty_env_keeps_essentials_and_drops_session_vars() {
        // `std::env::set_var` is unsafe (denied), so this reads the
        // ambient env and asserts on the curated allowlist instead.
        let mut extra = std::collections::HashMap::new();
        extra.insert("_TAB_ID".to_string(), "abc-123".to_string());
        let env = minimal_pty_env(true, &std::collections::BTreeMap::new(), &extra);

        // PATH is always present (allowlisted, or the default fallback).
        assert!(env.get("PATH").is_some_and(|p| !p.is_empty()), "PATH must be set");
        // Colours come from the flag, not the parent.
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        // Telemetry opt-out is folded in.
        assert_eq!(env.get("DO_NOT_TRACK").map(String::as_str), Some("1"));
        // Per-tab extras pass through.
        assert_eq!(env.get("_TAB_ID").map(String::as_str), Some("abc-123"));
        // Session / sensitive vars are NEVER carried over (the point of
        // the feature) — they're not on CLEAR_ENV_KEEP, so even if the
        // test host has them set they must be absent here.
        for leaky in [
            "DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "SSH_AUTH_SOCK",
            "XAUTHORITY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!env.contains_key(leaky), "{leaky} must not leak into a cleared-env tab");
        }
    }

    /// Reference reflected bit-by-bit CRC32 used to cross-check the
    /// table path in `crc32_matches_known_vector_and_is_stable`. Hoisted
    /// out of the test body so clippy's `items_after_statements` lint
    /// stays clean.
    fn bitwise_crc32(data: &[u8]) -> u32 {
        const POLY: u32 = 0xEDB8_8320;
        let mut crc: u32 = !0;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (POLY & mask);
            }
        }
        !crc
    }

    #[test]
    fn crc32_matches_known_vector_and_is_stable() {
        // IEEE CRC32 of "123456789" is the canonical check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        for sample in [b"".as_slice(), b"a", b"hello world", b"\x00\xff\x10tab", &[0u8; 257]] {
            assert_eq!(crc32(sample), bitwise_crc32(sample), "mismatch for {sample:?}");
        }
    }

    #[test]
    fn minimal_pty_env_uses_dumb_term_without_colors() {
        let env = minimal_pty_env(
            false,
            &std::collections::BTreeMap::new(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
        assert!(
            !env.contains_key("COLORTERM"),
            "no truecolor advertised when colours are off"
        );
    }

    #[test]
    fn minimal_pty_env_user_vars_win_over_basics() {
        // User settings override the kept basics and colours, but NOT
        // the per-tab API extras (those are applied last, functional).
        let mut user = std::collections::BTreeMap::new();
        user.insert("PATH".to_string(), "/opt/custom/bin".to_string());
        user.insert("TERM".to_string(), "screen-256color".to_string());
        user.insert("EDITOR".to_string(), "hx".to_string());
        let mut extra = std::collections::HashMap::new();
        extra.insert("_TAB_ID".to_string(), "tab-9".to_string());
        let env = minimal_pty_env(true, &user, &extra);
        // User wins over the basics/colours.
        assert_eq!(env.get("PATH").map(String::as_str), Some("/opt/custom/bin"));
        assert_eq!(env.get("TERM").map(String::as_str), Some("screen-256color"));
        // Brand-new user var lands.
        assert_eq!(env.get("EDITOR").map(String::as_str), Some("hx"));
        // Functional per-tab var is not clobbered by user settings.
        assert_eq!(env.get("_TAB_ID").map(String::as_str), Some("tab-9"));
    }

    #[test]
    fn clear_env_shell_command_is_env_dash_i_login_shell() {
        let mut env = std::collections::HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/u".to_string());
        let (prog, args) = clear_env_shell_command("/bin/zsh", true, &env);
        assert_eq!(prog, "/usr/bin/env");
        assert_eq!(
            args.first().map(String::as_str),
            Some("-i"),
            "must clear the environment"
        );
        assert!(args.iter().any(|a| a == "PATH=/usr/bin"));
        assert!(args.iter().any(|a| a == "HOME=/home/u"));
        // login=true ⇒ shell + `-l` are the final two args, so `env`
        // execs `/bin/zsh -l`.
        let n = args.len();
        assert_eq!(args[n - 2], "/bin/zsh");
        assert_eq!(args[n - 1], "-l");
        // login=false ⇒ the shell is the last arg, no `-l`.
        let (_, args_no_login) = clear_env_shell_command("/bin/sh", false, &env);
        assert_eq!(args_no_login.last().map(String::as_str), Some("/bin/sh"));
        assert!(!args_no_login.iter().any(|a| a == "-l"));
    }

    #[test]
    fn telemetry_disable_env_forces_all_optouts() {
        let mut env = std::collections::HashMap::new();
        // A pre-existing conflicting value must be FORCED to the opt-out.
        env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(), "0".to_string());
        apply_telemetry_disable_env(&mut env);
        for (k, v) in TELEMETRY_DISABLE_ENV {
            assert_eq!(env.get(*k).map(String::as_str), Some(*v), "{k} must be forced to {v}");
        }
        // The four expected opt-out switches are all present.
        assert_eq!(
            env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("DISABLE_TELEMETRY").map(String::as_str), Some("1"));
        assert_eq!(env.get("DO_NOT_TRACK").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY").map(String::as_str),
            Some("1")
        );
        // We must NOT re-enable the survey for OTEL collectors.
        assert!(!env.contains_key("CLAUDE_CODE_ENABLE_FEEDBACK_SURVEY_FOR_OTEL"));
    }

    #[test]
    fn load_state_at_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.json");
        let big = vec![b' '; (MAX_STATE_FILE_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        assert!(load_state_at(&path).is_none(), "oversized state file must be refused");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_with_rotation_sets_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        write_atomic_with_rotation(dir.path(), &path, &serde_json::json!({"token": "abc"}), false);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file must be owner-only, got {mode:o}");
    }

    #[test]
    fn strip_ansi_removes_sgr_and_keeps_text() {
        let s = "\x1b[1;31mhello\x1b[0m, \x1b[32mworld\x1b[0m";
        assert_eq!(strip_ansi(s), "hello, world");
    }

    #[test]
    fn strip_ansi_handles_no_escapes_and_partial_sequence() {
        assert_eq!(strip_ansi("plain text"), "plain text");
        // Lone ESC without `[` is preserved verbatim.
        assert_eq!(strip_ansi("ab\x1bcd"), "ab\x1bcd");
    }

    #[test]
    fn test_tab_state_serialization() {
        let state = SavedState {
            tabs: vec![
                TabState {
                    name: "Terminal".into(),
                    cwd: Some("/home/user".into()),
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                    ..Default::default()
                },
                TabState {
                    name: "Build".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                    ..Default::default()
                },
            ],
            active: 1,
            windowed: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.tabs[0].name, "Terminal");
        assert_eq!(restored.tabs[0].cwd, Some("/home/user".into()));
        assert_eq!(restored.tabs[1].name, "Build");
        assert_eq!(restored.tabs[1].cwd, None);
        assert_eq!(restored.active, 1);
    }

    #[test]
    fn tab_state_persists_last_used_at() {
        // last_used_at must round-trip through tabs.json so the MRU (Ctrl+P /
        // mobile) ordering survives a restart — the "persist on reboot" bug.
        let ts = TabState {
            name: "Agent".into(),
            last_used_at: Some(1_788_000_000_000),
            ..Default::default()
        };
        let json = serde_json::to_string(&ts).unwrap();
        assert!(json.contains("last_used_at"), "field must serialize: {json}");
        let back: TabState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_used_at, Some(1_788_000_000_000));
        // An older tabs.json without the field loads as None (re-seeds on focus).
        let legacy: TabState = serde_json::from_str(r#"{"id":"x","name":"Old","cwd":null}"#).unwrap();
        assert_eq!(legacy.last_used_at, None);
    }

    #[test]
    fn test_tab_state_colors_enabled_round_trip() {
        // false survives a round-trip; true is omitted from the JSON.
        let state = SavedState {
            tabs: vec![TabState {
                name: "dumb".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: false,
                tokens: None,
                ..Default::default()
            }],
            active: 0,
            windowed: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains("\"colors_enabled\":false"),
            "expected colors_enabled=false in {json}",
        );
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!(!restored.tabs[0].colors_enabled);

        // Missing field deserializes to the default (true).
        let restored: SavedState = serde_json::from_str(r#"{"tabs":[{"name":"x","cwd":null}],"active":0}"#).unwrap();
        assert!(restored.tabs[0].colors_enabled);
    }

    #[test]
    fn test_tab_state_uptime_energy_round_trip() {
        let state = SavedState {
            tabs: vec![TabState {
                name: "T".into(),
                cwd: None,
                output: None,
                uptime_secs: Some(123.5),
                energy_wh: Some(0.042),
                colors_enabled: true,
                tokens: None,
                ..Default::default()
            }],
            active: 0,
            windowed: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!((restored.tabs[0].uptime_secs.unwrap() - 123.5).abs() < f64::EPSILON);
        assert!((restored.tabs[0].energy_wh.unwrap() - 0.042).abs() < f64::EPSILON);
    }

    #[test]
    fn test_tab_state_uptime_energy_defaults() {
        let json = r#"{"tabs":[{"name":"X","cwd":null}],"active":0}"#;
        let restored: SavedState = serde_json::from_str(json).unwrap();
        assert!(restored.tabs[0].uptime_secs.is_none());
        assert!(restored.tabs[0].energy_wh.is_none());
    }

    #[test]
    fn test_tab_state_empty_tabs() {
        let state = SavedState {
            tabs: vec![],
            active: 0,
            windowed: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!(restored.tabs.is_empty());
    }

    #[test]
    fn test_state_path_uses_base() {
        let path = state_path(std::path::Path::new("/tmp/test-base"));
        assert!(path.ends_with(format!("{APP_DIR}/tabs.json")));
    }

    #[test]
    fn test_load_state_missing_file() {
        let result = load_state_from(std::path::Path::new("/tmp/ta-test-nonexistent"));
        assert!(result.is_none());
    }

    #[test]
    fn test_crc32_matches_known_vector() {
        // "123456789" → 0xCBF43926 (standard CRC-32/ISO-HDLC test vector).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn test_sanitize_tab_filename_collision_resistant() {
        // "foo/bar" and "foo_bar" sanitize to the same prefix but the CRC
        // suffix keeps them distinct.
        let a = sanitize_tab_filename("foo/bar");
        let b = sanitize_tab_filename("foo_bar");
        assert!(a.starts_with("foo_bar-"));
        assert!(b.starts_with("foo_bar-"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_sanitize_tab_filename_handles_unusual_names() {
        assert!(sanitize_tab_filename("").starts_with("_-"));
        assert!(sanitize_tab_filename(".hidden").starts_with("_.hidden-"));
        let long = "a".repeat(200);
        let san = sanitize_tab_filename(&long);
        // Truncated to 100 + 1 ("-") + 8 (hex) = 109 chars max.
        assert!(san.len() <= 109);
    }

    #[test]
    fn test_save_tab_output_round_trip() {
        let base = std::env::temp_dir().join("ta-test-output-roundtrip");
        let _ = std::fs::remove_dir_all(&base);

        save_tab_output(&base, "build/run", "lots of output\nhere\n");
        let loaded = load_tab_output(&base, "build/run");
        assert_eq!(loaded.as_deref(), Some("lots of output\nhere\n"));

        // Same sanitized prefix, different CRC → independent file.
        save_tab_output(&base, "build_run", "different tab");
        assert_eq!(load_tab_output(&base, "build_run").as_deref(), Some("different tab"));
        // Original is untouched.
        assert_eq!(
            load_tab_output(&base, "build/run").as_deref(),
            Some("lots of output\nhere\n")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_save_rotates_backups() {
        let dir = std::env::temp_dir().join("ta-test-rotation");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let mk = |name: &str| SavedState {
            tabs: vec![TabState {
                name: name.into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
                ..Default::default()
            }],
            active: 0,
            windowed: false,
        };

        save_state(&dir, &mk("v1"));
        save_state(&dir, &mk("v2"));
        save_state(&dir, &mk("v3"));
        save_state(&dir, &mk("v4"));

        let sd = state_dir(&dir);
        let read = |name: &str| {
            std::fs::read_to_string(sd.join(name))
                .ok()
                .and_then(|s| serde_json::from_str::<SavedState>(&s).ok())
                .and_then(|s| s.tabs.into_iter().next().map(|t| t.name))
        };

        assert_eq!(read("tabs.json").as_deref(), Some("v4"));
        assert_eq!(read("tabs.json.bak").as_deref(), Some("v3"));
        assert_eq!(read("tabs.json.bak.1").as_deref(), Some("v2"));
        assert_eq!(read("tabs.json.bak.2").as_deref(), Some("v1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_falls_back_to_bak_when_primary_corrupt() {
        let dir = std::env::temp_dir().join("ta-test-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        let sd = state_dir(&dir);
        let _ = std::fs::create_dir_all(&sd);

        let good = SavedState {
            tabs: vec![TabState {
                name: "rescued".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
                ..Default::default()
            }],
            active: 0,
            windowed: false,
        };
        std::fs::write(sd.join("tabs.json"), "broken json").unwrap();
        std::fs::write(sd.join("tabs.json.bak"), serde_json::to_string(&good).unwrap()).unwrap();

        let loaded = load_state_from(&dir).expect("should fall back to .bak");
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].name, "rescued");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_then_load_round_trip() {
        let dir = std::env::temp_dir().join("ta-test-round-trip");
        let _ = std::fs::create_dir_all(&dir);

        let state = SavedState {
            tabs: vec![
                TabState {
                    name: "One".into(),
                    cwd: Some("/tmp".into()),
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                    ..Default::default()
                },
                TabState {
                    name: "Two".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                    ..Default::default()
                },
            ],
            active: 1,
            windowed: false,
        };
        save_state(&dir, &state);
        let loaded = load_state_from(&dir).expect("should load saved state");
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.tabs[0].name, "One");
        assert_eq!(loaded.tabs[0].cwd, Some("/tmp".into()));
        assert_eq!(loaded.tabs[1].name, "Two");
        assert_eq!(loaded.tabs[1].cwd, None);
        assert_eq!(loaded.active, 1);

        // The pre-serialized entry point (what the persist ticks call)
        // must round-trip identically to `save_state`.
        let mut renamed = state;
        renamed.tabs[0].name = "One-renamed".into();
        let json = serde_json::to_string_pretty(&renamed).unwrap();
        save_state_serialized(&dir, &json);
        let loaded = load_state_from(&dir).expect("should load pre-serialized state");
        assert_eq!(loaded.tabs[0].name, "One-renamed");
        assert_eq!(loaded.active, 1);

        let _ = std::fs::remove_dir_all(dir.join(APP_DIR));
    }

    #[test]
    fn load_state_with_outputs_hydrates_per_tab_files() {
        let dir = std::env::temp_dir().join("ta-test-hydrate");
        let _ = std::fs::remove_dir_all(&dir);
        let mk = |name: &str| TabState {
            name: name.into(),
            colors_enabled: true,
            ..Default::default()
        };
        save_state(
            &dir,
            &SavedState {
                tabs: vec![mk("one"), mk("two"), mk("three")],
                active: 0,
                windowed: false,
            },
        );
        save_tab_output(&dir, "one", "hello from one");
        save_tab_uptime(&dir, "two", 42.5);

        // Multiple tabs ⇒ the threaded fan-out path runs.
        let loaded = load_state_with_outputs(&dir, &dir).expect("state loads");
        assert_eq!(loaded.tabs[0].output.as_deref(), Some("hello from one"));
        assert!(loaded.tabs[0].uptime_secs.is_none(), "no uptime file for 'one'");
        assert!((loaded.tabs[1].uptime_secs.unwrap() - 42.5).abs() < 0.01);
        assert!(loaded.tabs[2].output.is_none(), "no files at all for 'three'");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_state_malformed_json() {
        let dir = std::env::temp_dir().join("ta-test-malformed");
        let sd = dir.join(APP_DIR);
        let _ = std::fs::create_dir_all(&sd);
        std::fs::write(sd.join("tabs.json"), "not json").unwrap();

        let result = load_state_from(&dir);
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&sd);
    }

    #[test]
    fn test_state_dir_has_app_dir() {
        let dir = state_dir(std::path::Path::new("/tmp/test"));
        assert_eq!(dir.file_name().unwrap(), APP_DIR);
    }

    #[test]
    fn test_state_dir_with_base() {
        let dir = state_dir(std::path::Path::new("/tmp/custom-state"));
        assert_eq!(dir, PathBuf::from(format!("/tmp/custom-state/{APP_DIR}")));
    }

    #[test]
    fn test_font_config_default() {
        let fc = FontConfig::default();
        assert_eq!(fc.family, "monospace");
        assert_eq!(fc.weight, 400);
        assert!((fc.size - 16.0).abs() < f32::EPSILON);
        assert!((fc.scroll_sensitivity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_font_config_preferences_win_over_zed_and_fontconfig() {
        // No zed/settings.json at this base ⇒ tier 2 yields the
        // "monospace" default; preferences.json (tier 1) must override
        // family + size outright, and because a concrete family is set,
        // the fontconfig fallback is never consulted.
        let base = std::path::Path::new("/tmp/ta-nonexistent-cfg-xyz");
        let prefs = Preferences {
            font_family: Some("JetBrains Mono".into()),
            font_size: Some(13.5),
            ..Default::default()
        };
        let fc = resolve_font_config(base, &prefs);
        assert_eq!(fc.family, "JetBrains Mono");
        assert!((fc.size - 13.5).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_font_config_without_overrides_is_never_the_bare_generic_when_fc_present() {
        // With no prefs and no zed settings, we must not leave gpui the
        // bare "monospace" alias *if* fontconfig is available — it gets
        // resolved to a concrete family. On a box without fc-match it
        // stays "monospace"; either way the family is non-empty.
        let base = std::path::Path::new("/tmp/ta-nonexistent-cfg-xyz");
        let fc = resolve_font_config(base, &Preferences::default());
        assert!(!fc.family.trim().is_empty());
    }

    #[test]
    fn test_load_font_config_missing_file() {
        let fc = load_font_config_from(std::path::Path::new("/tmp/nonexistent-config.json"));
        assert_eq!(fc.family, "monospace");
        assert_eq!(fc.weight, 400);
    }

    #[test]
    fn test_load_font_config_partial() {
        let dir = std::env::temp_dir().join("ta-test-font");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "ui_font_family": "JetBrains Mono", "ui_font_size": 14 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert_eq!(fc.family, "JetBrains Mono");
        assert!((fc.size - 14.0).abs() < f32::EPSILON);
        assert_eq!(fc.weight, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_buffer_font_fallback() {
        let dir = std::env::temp_dir().join("ta-test-font-fallback");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "buffer_font_size": 20 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.size - 20.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity() {
        let dir = std::env::temp_dir().join("ta-test-scroll-sens");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "scroll_sensitivity": 2.5 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.scroll_sensitivity - 2.5).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity_clamped() {
        let dir = std::env::temp_dir().join("ta-test-scroll-clamp");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "scroll_sensitivity": 0.001 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.scroll_sensitivity - 0.01).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_strip_json_comments_line() {
        let input = r#"{
  // this is a comment
  "key": "value"
}"#;
        let out = strip_json_comments(input);
        assert!(!out.contains("comment"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn test_strip_json_comments_block() {
        let input = r#"{ /* block comment */ "a": 1 }"#;
        let out = strip_json_comments(input);
        assert!(!out.contains("block"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn test_strip_json_comments_preserves_strings() {
        let input = r#"{ "url": "https://example.com" }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://example.com");
    }

    #[test]
    fn test_strip_json_comments_slash_in_string() {
        let input = r#"{ "path": "a//b", "x": 1 }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["path"], "a//b");
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn test_strip_json_comments_escaped_quote() {
        let input = r#"{ "s": "he said \"hi\"", "n": 1 }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["s"], r#"he said "hi""#);
    }

    #[test]
    fn test_load_font_config_with_comments() {
        let dir = std::env::temp_dir().join("ta-test-comments");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
  // font settings
  "ui_font_family": "Fira Code",
  "ui_font_weight": 700,
  "ui_font_size": 18
}"#,
        )
        .unwrap();
        let fc = load_font_config_from(&path);
        assert_eq!(fc.family, "Fira Code");
        assert_eq!(fc.weight, 700);
        assert!((fc.size - 18.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_state_creates_directory() {
        let dir = std::env::temp_dir().join("ta-test-create-dir");
        let _ = std::fs::remove_dir_all(&dir);
        let state = SavedState {
            tabs: vec![TabState {
                name: "T".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
                ..Default::default()
            }],
            active: 0,
            windowed: false,
        };
        save_state(&dir, &state);
        assert!(dir.join(format!("{APP_DIR}/tabs.json")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_http_url() {
        let urls = detect_urls("visit https://example.com/page today");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "https://example.com/page");
        assert!(!urls[0].3);
    }

    #[test]
    fn detect_http_url_with_query() {
        let urls = detect_urls("go to http://localhost:3000/api?key=val&x=1");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "http://localhost:3000/api?key=val&x=1");
    }

    #[test]
    fn detect_url_trims_trailing_punctuation() {
        let urls = detect_urls("see https://example.com.");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "https://example.com");
    }

    #[test]
    fn detect_single_slash_relative_file_path() {
        // Regression: a single-slash relative path with a filename
        // extension must be clickable (was missed — required >=2 slashes).
        let urls = detect_urls("POC saved at build/mangopay-birthday-poc.php");
        assert_eq!(urls.len(), 1, "got {urls:?}");
        assert_eq!(urls[0].2, "build/mangopay-birthday-poc.php");
        assert!(urls[0].3, "should be flagged as a path");

        let urls = detect_urls("edit src/main.rs now");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "src/main.rs");
    }

    #[test]
    fn single_slash_prose_is_not_a_path() {
        // The filename heuristic must reject prose / fractions / ratios.
        for s in ["choose and/or both", "uses TCP/IP here", "open 24/7", "ratio 1/2.5 ok"] {
            assert!(
                detect_urls(s).is_empty(),
                "false positive in {s:?}: {:?}",
                detect_urls(s)
            );
        }
        // …but a 2+-slash path is still detected regardless of extension.
        let urls = detect_urls("cd /usr/local/bin");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/usr/local/bin");
    }

    #[test]
    fn detect_urls_no_slash_short_circuits_empty() {
        // The allocation-free fast path: a line with no '/' cannot
        // contain any detectable URL or path. These all return empty.
        assert!(detect_urls("just some prose with no links at all").is_empty());
        assert!(detect_urls("123456 789012 numbers from seq").is_empty());
        assert!(detect_urls("# Xq9_-=Zb7A random paste line no slash").is_empty());
        assert!(detect_urls("https:example.com missing the slashes").is_empty());
        // And a line WITH a slash still detects normally (fast path
        // doesn't swallow real matches).
        assert_eq!(detect_urls("go https://x.io/p now").len(), 1);
    }

    #[test]
    fn detect_file_path() {
        let urls = detect_urls("error at /home/user/src/main.rs:42:5");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/home/user/src/main.rs:42:5");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_file_path_needs_two_components() {
        let urls = detect_urls("see /tmp or /dev");
        assert!(urls.is_empty());
    }

    #[test]
    fn detect_relative_path_strips_flag_prefix_and_allows_long_ext() {
        // dpkg glues its `-O` flag straight onto a relative path. The link must
        // start at `../` (NOT include `-O`), and `.buildinfo` (9-char ext) must
        // still link because the `../` marker is unambiguous — both lines the
        // user reported.
        let a = detect_urls(" dpkg-genbuildinfo --build=source -O../twig-i18n-extension_5.0.2-1_source.buildinfo");
        assert_eq!(a.len(), 1, "buildinfo line should detect exactly one path: {a:?}");
        assert_eq!(a[0].2, "../twig-i18n-extension_5.0.2-1_source.buildinfo");
        assert!(a[0].3, "should be flagged as a path");
        let b = detect_urls(" dpkg-genchanges -sa --build=source -O../twig-i18n-extension_5.0.2-1_source.changes");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].2, "../twig-i18n-extension_5.0.2-1_source.changes");
    }

    #[test]
    fn detect_relative_path_dot_slash_and_gcc_flag() {
        // Leading `./` with a long extension links despite the filename check.
        let u = detect_urls("wrote ./out.buildinfo now");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].2, "./out.buildinfo");
        // gcc-style `-I../include/foo.h`: path from `../`, flag `-I` dropped.
        let g = detect_urls("cc -I../include/foo.h x.c");
        assert!(g.iter().any(|d| d.2 == "../include/foo.h"), "got: {g:?}");
        assert!(
            !g.iter().any(|d| d.2.contains("-I")),
            "flag must not be in the link: {g:?}"
        );
    }

    #[test]
    fn detect_file_path_trims_trailing_period() {
        let urls = detect_urls("deb at /tmp/pkg/app_0.1-1_amd64.deb.");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/tmp/pkg/app_0.1-1_amd64.deb");
    }

    #[test]
    fn detect_file_path_trims_trailing_colon() {
        // grep / compiler / `ls -la` / traceback lines end paths with
        // `:` to delimit a line number or extra info. The colon isn't
        // part of the path itself — strip it.
        let urls = detect_urls("see /mnt/Dev/questionnaire.pdf: header missing");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/mnt/Dev/questionnaire.pdf");
        // Middle-of-path colons (line numbers) MUST survive — that's
        // the existing `detect_file_path` invariant.
        let urls = detect_urls("see /mnt/Dev/main.rs:42:5 column");
        assert_eq!(urls[0].2, "/mnt/Dev/main.rs:42:5");
    }

    #[test]
    fn detect_file_path_with_tilde() {
        let urls = detect_urls("see ~/.local/state/tab-atelier/tabs.json for state");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "~/.local/state/tab-atelier/tabs.json");
    }

    #[test]
    fn detect_file_path_with_tilde_at_start() {
        let urls = detect_urls("~/.config/foo/bar.txt");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "~/.config/foo/bar.txt");
    }

    #[test]
    fn detect_file_path_with_env_var_prefix() {
        let urls = detect_urls("see $XDG_STATE_HOME/tab-atelier/tabs.json after reboot");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "$XDG_STATE_HOME/tab-atelier/tabs.json");
    }

    #[test]
    fn detect_file_path_with_home_env_var() {
        let urls = detect_urls("$HOME/dev/foo/bar.rs");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "$HOME/dev/foo/bar.rs");
    }

    #[test]
    fn detect_file_path_does_not_eat_arbitrary_prefix() {
        // 'cat' before /tmp shouldn't be captured (only ~ is grafted on).
        let urls = detect_urls("cat /home/user/foo/bar");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/home/user/foo/bar");
    }

    #[test]
    fn detect_multiple_urls() {
        let urls = detect_urls("https://a.com and /home/user/file.rs");
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn file_path_strip_line_col() {
        assert_eq!(file_path_for_open("/src/main.rs:42:5"), "/src/main.rs");
        assert_eq!(file_path_for_open("/src/main.rs:42"), "/src/main.rs");
        assert_eq!(file_path_for_open("/src/main.rs"), "/src/main.rs");
    }

    #[test]
    fn no_urls_in_plain_text() {
        let urls = detect_urls("hello world nothing here");
        assert!(urls.is_empty());
    }

    #[test]
    fn detect_partial_path_with_line() {
        let urls = detect_urls("error at src/main.php:42");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "src/main.php:42");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_partial_path_with_line_col() {
        let urls = detect_urls("see src/lib/utils.rs:10:5 for details");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "src/lib/utils.rs:10:5");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_relative_path_with_prefix() {
        let urls = detect_urls("│ phpMyAdmin/2026/02/detailed-report.md |");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "phpMyAdmin/2026/02/detailed-report.md");
        assert!(urls[0].3);
    }

    #[test]
    fn file_path_for_open_partial() {
        assert_eq!(file_path_for_open("src/main.php:42"), "src/main.php");
        assert_eq!(file_path_for_open("src/lib/utils.rs:10:5"), "src/lib/utils.rs");
    }

    #[test]
    fn test_active_clamped_on_load() {
        let dir = std::env::temp_dir().join("ta-test-clamp-active");
        let sd = dir.join(APP_DIR);
        let _ = std::fs::create_dir_all(&sd);
        let state = SavedState {
            tabs: vec![TabState {
                name: "Only".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
                ..Default::default()
            }],
            active: 999,
            windowed: false,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(sd.join("tabs.json"), json).unwrap();

        let loaded = load_state_from(&dir).unwrap();
        assert_eq!(loaded.active, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpui_key_to_keycode_known_keys() {
        assert_eq!(gpui_key_to_keycode("`"), Some(49));
        assert_eq!(gpui_key_to_keycode("f12"), Some(96));
        assert_eq!(gpui_key_to_keycode("f1"), Some(67));
        assert_eq!(gpui_key_to_keycode("escape"), Some(9));
        assert_eq!(gpui_key_to_keycode("space"), Some(65));
        assert_eq!(gpui_key_to_keycode("a"), Some(38));
        assert_eq!(gpui_key_to_keycode("xf86calculator"), Some(148));
    }

    #[test]
    fn gpui_key_to_keycode_unknown() {
        assert_eq!(gpui_key_to_keycode("nonexistent"), None);
        assert_eq!(gpui_key_to_keycode(""), None);
        assert_eq!(gpui_key_to_keycode("F12"), None);
    }

    #[test]
    fn keycode_label_known() {
        assert_eq!(keycode_label(49), "` (Grave)");
        assert_eq!(keycode_label(96), "F12");
        assert_eq!(keycode_label(148), "XF86Calculator");
        assert_eq!(keycode_label(65), "Space");
    }

    #[test]
    fn keycode_label_unknown_fallback() {
        assert_eq!(keycode_label(200), "Key 200");
        assert_eq!(keycode_label(0), "Key 0");
        assert_eq!(keycode_label(255), "Key 255");
    }

    #[test]
    fn legacy_hotkey_ids() {
        assert_eq!(legacy_hotkey_id_to_keycode("grave"), Some(49));
        assert_eq!(legacy_hotkey_id_to_keycode("f1"), Some(67));
        assert_eq!(legacy_hotkey_id_to_keycode("f11"), Some(95));
        assert_eq!(legacy_hotkey_id_to_keycode("f12"), Some(96));
        assert_eq!(legacy_hotkey_id_to_keycode("xf86calculator"), Some(148));
        assert_eq!(legacy_hotkey_id_to_keycode("unknown"), None);
        assert_eq!(legacy_hotkey_id_to_keycode(""), None);
    }

    #[test]
    fn deserialize_hotkeys_numbers() {
        let json = r#"{"hotkeys": [49, 96, 148]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96, 148]);
    }

    #[test]
    fn deserialize_hotkeys_legacy_strings() {
        let json = r#"{"hotkeys": ["grave", "f12", "xf86calculator"]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96, 148]);
    }

    #[test]
    fn deserialize_hotkeys_mixed() {
        let json = r#"{"hotkeys": ["grave", 96]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96]);
    }

    #[test]
    fn deserialize_hotkeys_empty() {
        let json = r#"{"hotkeys": []}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert!(prefs.hotkeys.is_empty());
    }

    #[test]
    fn deserialize_hotkeys_missing_field() {
        let json = r"{}";
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert!(prefs.hotkeys.is_empty());
    }

    #[test]
    fn deserialize_hotkeys_invalid_entries_skipped() {
        let json = r#"{"hotkeys": ["grave", "bogus", null, 300, 49]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 49]);
    }

    #[test]
    fn deserialize_preferences_without_remote_endpoints_defaults_to_empty() {
        let json = r"{}";
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert!(prefs.remote_endpoints.is_empty());
    }

    #[test]
    fn serialize_preferences_skips_empty_remote_endpoints() {
        let prefs = Preferences::default();
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(
            !json.contains("remote_endpoints"),
            "expected remote_endpoints to be skipped when empty, got {json}"
        );
    }

    #[test]
    fn remote_endpoint_round_trip() {
        let prefs = Preferences {
            remote_endpoints: vec![
                RemoteEndpoint {
                    id: "11111111-2222-3333-4444-555555555555".into(),
                    label: "colossus".into(),
                    url: "https://192.168.1.42:7891".into(),
                    token: "deadbeef".into(),
                    cert_sha256: "a".repeat(64),
                    autoconnect: true,
                    cf_access_client_id: "svc.access".into(),
                    cf_access_client_secret: "s3cr3t".into(),
                    relay_token: "relay-only".into(),
                },
                RemoteEndpoint {
                    id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    label: "build-box".into(),
                    url: "http://127.0.0.1:7890".into(),
                    token: "feedface".into(),
                    cert_sha256: String::new(),
                    autoconnect: false,
                    ..Default::default()
                },
            ],
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let restored: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.remote_endpoints.len(), 2);
        assert_eq!(restored.remote_endpoints[0].label, "colossus");
        assert_eq!(restored.remote_endpoints[0].url, "https://192.168.1.42:7891");
        assert!(restored.remote_endpoints[0].autoconnect);
        // CF Access service token round-trips and flips has_cf_service_token().
        assert_eq!(restored.remote_endpoints[0].cf_access_client_id, "svc.access");
        assert_eq!(restored.remote_endpoints[0].cf_access_client_secret, "s3cr3t");
        assert!(restored.remote_endpoints[0].has_cf_service_token());
        assert_eq!(restored.remote_endpoints[1].label, "build-box");
        assert_eq!(restored.remote_endpoints[1].cert_sha256, "");
        assert!(!restored.remote_endpoints[1].autoconnect);
        // Absent CF token → empty pair, and the JSON omits the fields entirely.
        assert!(!restored.remote_endpoints[1].has_cf_service_token());
        assert!(
            !json.contains("cf_access_client_id") || json.matches("cf_access_client_id").count() == 1,
            "empty CF token must be skipped in serialization, got {json}"
        );
    }
}

#[cfg(test)]
mod state_writer_tests {
    use super::StateWriter;

    #[test]
    fn writes_run_in_order_and_flush_waits_for_them() {
        let w = StateWriter::spawn();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for i in 0..8 {
            let log = log.clone();
            w.submit(move || log.lock().unwrap().push(i));
        }
        w.flush();
        assert_eq!(
            *log.lock().unwrap(),
            (0..8).collect::<Vec<_>>(),
            "FIFO, all done at flush"
        );
        // A second flush with an empty queue returns immediately.
        w.flush();
    }
}
