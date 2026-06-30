// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

pub mod alloc_count;
pub(crate) mod api;
pub(crate) mod api_ws;
#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub(crate) mod box_drawing;
#[cfg(feature = "catbus")]
pub(crate) mod catbus_agent;
#[cfg(all(target_os = "linux", not(feature = "gui")))]
pub(crate) mod cgroup;
pub mod cli;
#[cfg(not(feature = "gui"))]
pub mod headless;
/// Experimental HTTP/3 + WebTransport transport (behind `http3`).
#[cfg(feature = "http3")]
pub mod http3;
pub(crate) mod locale;
pub mod net_policy;
pub mod net_proxy;
pub(crate) mod platform;
#[cfg(feature = "energy")]
pub(crate) mod power;
pub(crate) mod pty_ring;
pub mod remote;
pub mod schedule;
#[cfg(feature = "gui")]
pub(crate) mod screenshot;
pub(crate) mod term_export;
#[cfg(feature = "gui")]
pub(crate) mod terminal;
#[cfg(feature = "gui")]
pub(crate) mod terminal_utils;
pub(crate) mod theme;
pub(crate) mod tracking;
#[cfg(all(windows, not(feature = "gui")))]
pub mod win_service;

pub const APP_DIR: &str = "tab-atelier";

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
pub fn tab_env_extras(tab_id: &str, api_url: &str, api_token: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert("_TAB_ID".into(), tab_id.to_string());
    m.insert("TAB_ATELIER_API_URL".into(), api_url.to_string());
    m.insert("TAB_ATELIER_API_TOKEN".into(), api_token.to_string());
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
        _ => None,
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

/// Default viewer background — Tomorrow Night Blue. Softer than pitch
/// black; legible foreground contrast on most monitors.
pub const DEFAULT_TAB_BG_COLOR: &str = "#002451";

/// Resolve the effective background color for a tab: per-tab override
/// → global pref → Tomorrow Night Blue.
#[must_use]
pub fn effective_tab_bg<'a>(per_tab: Option<&'a str>, global: Option<&'a str>) -> &'a str {
    per_tab.or(global).unwrap_or(DEFAULT_TAB_BG_COLOR)
}

#[must_use]
pub fn default_tab_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
            output: None,
            uptime_secs: None,
            energy_wh: None,
            tokens: None,
            colors_enabled: true,
            agent_state: None,
            agent_session_id: None,
            agent_kind: None,
            agent_plan_mode: None,
            share_token_rw: String::new(),
            share_token_ro: String::new(),
            locked: false,
            net_disabled: false,
            net_allow_presets: Vec::new(),
            net_allow_domains: Vec::new(),
            net_allow_cidrs: Vec::new(),
            bg_color: None,
            schedule: None,
            limits: TabResourceLimits::default(),
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
#[must_use]
pub fn load_state_with_outputs(config_base: &std::path::Path, state_base: &std::path::Path) -> Option<SavedState> {
    let mut state = load_state_at(&config_state_path(config_base))?;
    for t in &mut state.tabs {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<u8>,
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
    pub token: String,
    /// Hex SHA-256 of the remote's TLS cert (TOFU-pinned).
    pub cert_sha256: String,
    /// When true, the GUI connects to this endpoint at startup
    /// instead of waiting for an explicit "Connect" click.
    #[serde(default)]
    pub autoconnect: bool,
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
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir);
    let result = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    let Ok(data) = result else {
        return;
    };

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
            if slashes >= 2 || single_slash_file {
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

        let _ = std::fs::remove_dir_all(dir.join(APP_DIR));
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
                },
                RemoteEndpoint {
                    id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    label: "build-box".into(),
                    url: "http://127.0.0.1:7890".into(),
                    token: "feedface".into(),
                    cert_sha256: String::new(),
                    autoconnect: false,
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
        assert_eq!(restored.remote_endpoints[1].label, "build-box");
        assert_eq!(restored.remote_endpoints[1].cert_sha256, "");
        assert!(!restored.remote_endpoints[1].autoconnect);
    }
}
