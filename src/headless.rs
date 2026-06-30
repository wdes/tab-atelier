// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless tab-atelier entry point.
//!
//! Restores every tab from `tabs.json`, spawns its PTY through
//! `alacritty_terminal::tty`, runs the same local HTTP / TLS API
//! the desktop GUI uses, and persists output / uptime / energy /
//! token state on a 2 Hz tick. No display server, no gpui, no
//! x11rb — just libc + alacritty + rustls.
//!
//! Drains the same pending-action queues the GUI's `persist()` does
//! (closes / activate / input / rename / status updates / new-tab
//! requests) so anything that talks to `/tabs/*` keeps working
//! identically against this binary.

#![cfg(not(feature = "gui"))]

use crate::{api_url_for_local_clients, build_agent_resume_command, tab_env_extras};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use log::{debug, info, warn};

use crate::api;
use crate::platform;
#[cfg(feature = "energy")]
use crate::save_tab_energy;
use crate::{
    AgentStateSnapshot, DEFAULT_API_ADDR, DEFAULT_API_TLS_ADDR, SHUTDOWN_REQUESTED, SavedState, TabState, crc32,
    default_tab_id, load_preferences, load_state_with_outputs, save_state, save_tab_output, save_tab_uptime,
};

const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;

// Shared with the GUI — see `crate::tab_env_extras`,
// `crate::api_url_for_local_clients`, and
// `crate::build_agent_resume_command` in lib.rs.

/// Tiny `EventListener` that just keeps the PTY-reply channel hooked
/// up. Same shape as `terminal.rs::EventProxy` minus the gpui-side
/// notify call.
#[derive(Clone, Default)]
struct EventProxy {
    notifier: Arc<Mutex<Option<EventLoopSender>>>,
}

impl EventProxy {
    fn set_notifier(&self, sender: EventLoopSender) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(sender);
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        if let AlacrittyEvent::PtyWrite(text) = event
            && let Ok(slot) = self.notifier.lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(Msg::Input(text.into_bytes().into()));
        }
    }
}

use crate::term_export::TermDims;

/// Per-tab headless state. Mirrors the persistable fields of the
/// GUI's `Tab` plus an owned PTY handle. Anything that doesn't
/// participate in tabs.json / the API snapshot is intentionally
/// missing (no font config, no focus, no scrollbar drag flag, …).
struct HeadlessTab {
    id: String,
    name: String,
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: EventLoopSender,
    #[allow(dead_code)]
    event_proxy: EventProxy,
    pid: u32,
    /// Wall-clock at which this tab's PTY was spawned in *this*
    /// process run. `prior_uptime` folds in time accumulated in
    /// previous runs so a restart doesn't reset the counter.
    created_at: Instant,
    prior_uptime: Duration,
    active_duration: Duration,
    last_activated: Option<Instant>,
    last_input: Option<Instant>,
    #[cfg(feature = "energy")]
    energy_wh: f64,
    #[cfg(feature = "energy")]
    energy_wh_last_saved: f64,
    output_hash_last_saved: u32,
    /// PTY-ring `total_len` at the last output save — same dirtiness
    /// gate as the GUI's `Tab`: skip the expensive `copy_all_history`
    /// when no new bytes reached the grid. crc32 stays authoritative.
    output_ring_len_last_saved: Option<u64>,
    pending_restore: Option<String>,
    last_known_cwd: Option<PathBuf>,
    last_known_cwd_string: Option<String>,
    agent_state: Option<AgentStateSnapshot>,
    agent_session_id: Option<String>,
    agent_kind: Option<String>,
    agent_plan_mode: Option<bool>,
    share_token_rw: String,
    share_token_ro: String,
    /// Manual lock — user-toggled via right-click / `POST /lock`.
    /// **Gate authors:** read [`crate::schedule::LockState::effective_locked`]
    /// (via `tab.effective_locked()`) instead of this raw field.
    locked: bool,
    schedule: Option<crate::schedule::TabSchedule>,
    bg_color: Option<String>,
    /// When true the shell runs inside a bubblewrap netns (no internet).
    /// Persisted; applied on (re)spawn.
    net_disabled: bool,
    /// Allowlist-mode config (mutually exclusive with `net_disabled`).
    /// Persisted; applied on (re)spawn by installing per-tab nftables rules
    /// (CIDRs) before the shell starts. Empty ⇒ not in allowlist mode.
    net_allow: crate::net_policy::AllowConfig,
    /// Free-text context the in-tab agent set via `set-context`.
    /// In-memory only (not persisted); reflected on `/tabs`.
    context: Option<String>,
    pending_agent_resume: Option<String>,
    colors_enabled: bool,
    /// Raw PTY byte ring captured BEFORE alacritty's parser sees the
    /// bytes. Source of truth for the share-link viewer's scrollback
    /// — alacritty's grid history is wiped by `\x1b[3J` and never
    /// grows when the TUI redraws in-place (Claude Code, htop, …).
    pty_ring: Arc<Mutex<crate::pty_ring::PtyRing>>,
    /// Memoised grid-derived snapshot fields, keyed by the PTY ring's
    /// monotonic `total_len`. `refresh_snapshot` runs every tick and
    /// the grid scans (`ansi_text_with_cursor(200)` + 2000-row
    /// `raw_screen_text`) are the bulk of its cost; since every byte
    /// that can change the grid flows through the ring, a `total_len`
    /// that hasn't advanced means the grid is byte-for-byte identical
    /// and the previous scan can be reused. `None` until the first scan.
    snap_cache: Option<crate::term_export::GridSnapshotCache>,
    /// Per-tab resource-limit overrides, carried so `persist()` writes
    /// them back to `tabs.json` instead of wiping them each tick.
    limits: crate::TabResourceLimits,
}

impl crate::schedule::LockState for HeadlessTab {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&crate::schedule::TabSchedule> {
        self.schedule.as_ref()
    }
}

impl HeadlessTab {
    fn uptime(&self) -> Duration {
        let live = self.last_activated.map(|t| t.elapsed()).unwrap_or_default();
        self.prior_uptime + self.active_duration + live
    }

    fn activate(&mut self) {
        if self.last_activated.is_none() {
            self.last_activated = Some(Instant::now());
        }
    }

    fn deactivate(&mut self) {
        if let Some(t) = self.last_activated.take() {
            self.active_duration += t.elapsed();
        }
    }

    fn send_input_bytes(&mut self, bytes: Vec<u8>) {
        // Defense in depth: the /input HTTP endpoint already calls
        // `effective_locked()` before pushing into pending_input, so
        // this branch only fires if some future code path bypasses
        // the API gate (a new endpoint, a direct test fixture, …).
        // Refuse rather than silently dropping so the call site
        // surfaces the misuse during development.
        if crate::schedule::LockState::effective_locked(self) {
            log::warn!(
                "send_input_bytes called on locked tab {} — dropping {} bytes",
                self.id,
                bytes.len()
            );
            return;
        }
        self.last_input = Some(Instant::now());
        let _ = self.notifier.send(Msg::Input(bytes.into()));
    }

    fn restore_output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
        let mut term = self.term.lock();
        for line in text.lines() {
            parser.advance(&mut *term, line.as_bytes());
            parser.advance(&mut *term, b"\r\n");
        }
    }

    fn flush_pending_restore(&mut self) {
        if let Some(out) = self.pending_restore.take() {
            self.restore_output(&out);
            // restore_output feeds the parser directly (not through the
            // PTY ring), so the ring's total_len doesn't move — drop the
            // snapshot cache so the next refresh re-scans the restored grid.
            self.snap_cache = None;
        }
    }

    fn flush_pending_agent_resume(&mut self) {
        if let Some(cmd) = self.pending_agent_resume.take() {
            self.send_input_bytes(vec![0x15]); // Ctrl-U
            let mut bytes = cmd.into_bytes();
            bytes.push(b'\n');
            self.send_input_bytes(bytes);
        }
    }

    fn shutdown(&self) {
        let _ = self.notifier.send(Msg::Shutdown);
    }

    /// Snapshot the scrollback + visible screen as ANSI text.
    /// Mirrors the structure of `TerminalView::ansi_lines` but
    /// without any gpui dependency. Returns (joined-output,
    /// optional-cursor-position). Delegates to the shared
    /// `term_export` so the GUI and headless paths can't drift.
    fn ansi_text_with_cursor(&self, max_lines: Option<usize>) -> (String, Option<(usize, usize)>) {
        crate::term_export::term_to_ansi_text_with_cursor(&self.term, max_lines)
    }

    fn raw_screen_text(&self, max_lines: Option<usize>) -> (String, Option<(usize, usize)>) {
        crate::term_export::term_to_ansi_rows(&self.term, max_lines)
    }

    fn dims(&self) -> (u16, u16) {
        let t = self.term.lock();
        let g = t.grid();
        let cols = g.columns() as u16;
        let rows = g.screen_lines() as u16;
        drop(t);
        (cols, rows)
    }

    /// Current PTY-ring high-water mark — the dirtiness key for the
    /// snapshot cache. A value equal to the last cached one means no
    /// new bytes reached alacritty, so the grid is unchanged.
    fn ring_total_len(&self) -> u64 {
        self.pty_ring.lock().map_or(0, |r| r.total_len())
    }

    /// Return the grid-derived snapshot fields, scanning the terminal
    /// only when the PTY ring advanced since the last call. Otherwise
    /// the previous scan is reused, avoiding the per-tick full-grid
    /// walk on idle tabs.
    fn cached_grid(&mut self) -> &crate::term_export::GridSnapshotCache {
        let ring_len = self.ring_total_len();
        if self.snap_cache.as_ref().is_none_or(|c| c.ring_len != ring_len) {
            let (output, cursor) = self.ansi_text_with_cursor(Some(200));
            let (raw_output, raw_cursor) = self.raw_screen_text(Some(2000));
            let (cols, rows) = self.dims();
            self.snap_cache = Some(crate::term_export::GridSnapshotCache {
                ring_len,
                output,
                cursor,
                raw_output,
                raw_cursor,
                cols,
                rows,
            });
        }
        self.snap_cache.as_ref().expect("snap_cache populated above")
    }

    fn copy_all_history(&self) -> String {
        self.ansi_text_with_cursor(None).0
    }
}

fn pty_env(colors_enabled: bool) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if colors_enabled {
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
    } else {
        env.insert("TERM".into(), "dumb".into());
    }
    // Force the telemetry / feedback-survey opt-out onto every tab.
    crate::apply_telemetry_disable_env(&mut env);
    env
}

#[allow(clippy::too_many_arguments)]
fn spawn_pty_tab(
    id: String,
    name: String,
    cwd: Option<PathBuf>,
    colors_enabled: bool,
    extra_env: HashMap<String, String>,
    prior_uptime_secs: f64,
    energy_wh: f64,
    saved_output_hash: u32,
    pending_restore: Option<String>,
    agent_session_id: Option<String>,
    agent_kind: Option<String>,
    agent_plan_mode: Option<bool>,
    share_token_rw: String,
    share_token_ro: String,
    locked: bool,
    schedule: Option<crate::schedule::TabSchedule>,
    bg_color: Option<String>,
    pty_cols: usize,
    pty_rows: usize,
    net_disabled: bool,
    net_allow: crate::net_policy::AllowConfig,
) -> Option<HeadlessTab> {
    let ws = WindowSize {
        num_lines: pty_rows as u16,
        num_cols: pty_cols as u16,
        cell_width: 9,
        cell_height: 18,
    };
    // Pick the shell explicitly. Alacritty defaults to the user's
    // login shell from /etc/passwd. The headless deb creates
    // `tab-atelier` as a system user with `nologin` as its login
    // shell (correct hardening — nobody should `su` into the
    // daemon's account), so without overriding here, every spawned
    // PTY would print "this account is currently not available"
    // and exit. Prefer /bin/bash, fall back to /bin/sh.
    #[cfg(unix)]
    let shell_program: String = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash".to_string()
    } else {
        "/bin/sh".to_string()
    };
    // On Windows there is no equivalent of the passwd shell — alacritty's
    // ConPTY backend just calls CreateProcess on whatever we hand it. Use
    // %COMSPEC% (the canonical env-pointer at cmd.exe) when set, fall
    // back to the well-known path. Without this override the Linux-style
    // `/bin/sh` reaches alacritty and CreateProcess returns ERROR_FILE_NOT_FOUND
    // (os error 2) — the bug the CI Windows self-test caught.
    #[cfg(windows)]
    let shell_program: String = std::env::var("COMSPEC")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".to_string());
    let (env_map, prog, args): (HashMap<String, String>, String, Vec<String>) = if crate::clear_env() {
        // Cleared-env mode: spawn the shell through `env -i` so it
        // inherits NOTHING from the daemon — only the curated minimal
        // allowlist (PATH/HOME/USER/locale + colours + telemetry opt-out
        // + the per-tab API vars). `login = false` matches the no-`-l`
        // choice below for the service account. `Options.env` stays
        // empty because `env -i` discards alacritty's overlaid vars.
        let min_env = crate::minimal_pty_env(colors_enabled, crate::clear_env_user_vars(), &extra_env);
        let (p, a) = crate::clear_env_shell_command(&shell_program, false, &min_env);
        (HashMap::new(), p, a)
    } else {
        // Inherit the daemon environment, then overlay colours +
        // telemetry opt-out (pty_env) and the per-tab API vars. No `-l`:
        // a login shell would source /etc/profile + ~/.profile, which
        // under ProtectHome=true can fail noisily for the service account.
        let mut env = pty_env(colors_enabled);
        env.extend(extra_env);
        (env, shell_program, vec![])
    };
    // Install the kernel egress allowlist BEFORE the shell exists, so the
    // tab is confined the instant it's moved into its cgroup (no unconfined
    // window — the cgroup is created empty + rules applied here; the pid
    // joins it right after spawn). teardown-first un-confines a tab that
    // left allowlist mode. CIDR-only — domain allowlists are the resolver's
    // job (nft can't match a hostname). Best-effort: logs + leaves the tab
    // unconfined on failure.
    #[cfg(target_os = "linux")]
    let nft_cgroup: Option<String> = {
        crate::net_nft::teardown(&id);
        if net_disabled || net_allow.is_empty() {
            None
        } else {
            let cidrs = net_allow.to_allow_set().cidrs;
            if cidrs.is_empty() {
                log::debug!("net_nft: tab '{name}' allowlist has no CIDRs; nothing for nft to confine yet");
                None
            } else if let Some(rel) = crate::cgroup::prepare_tab_cgroup(&id) {
                if crate::net_nft::apply(&id, &rel, &cidrs) {
                    Some(rel)
                } else {
                    None
                }
            } else {
                log::debug!("net_nft: no per-tab cgroup for '{name}'; kernel enforcement skipped");
                None
            }
        }
    };

    // Net-off tabs run their shell inside a bubblewrap netns (which also
    // drops all caps). Otherwise strip Linux capabilities from the tab's
    // subtree via setpriv — the daemon may hold CAP_NET_ADMIN to program
    // nft, and the agent must NOT inherit it (it could `nft flush` its own
    // allowlist and escape).
    let (prog, args) = if net_disabled {
        crate::no_internet_command(&prog, &args)
    } else if crate::setpriv_available() {
        crate::drop_caps_command(&prog, &args)
    } else {
        (prog, args)
    };
    let opts = tty::Options {
        shell: Some(tty::Shell::new(prog, args)),
        working_directory: cwd.clone(),
        env: env_map,
        ..Default::default()
    };
    let pty = match tty::new(&opts, ws, 0) {
        Ok(p) => p,
        Err(e) => {
            warn!("headless: pty spawn failed for '{name}': {e}");
            return None;
        }
    };
    #[cfg(unix)]
    let pid = pty.child().id();
    // ConPTY's Pty doesn't expose the child the way the Unix one does.
    // Every PID consumer (catbus, energy, /proc cwd) is disabled on
    // Windows, so a sentinel keeps the build going until a real ConPTY
    // child-PID lookup is wired up.
    #[cfg(windows)]
    let pid = 0u32;
    // Join the (already nft-confined) cgroup immediately so enforcement
    // begins right away. The rules were installed pre-spawn above.
    #[cfg(target_os = "linux")]
    if nft_cgroup.is_some() {
        crate::cgroup::move_pid_to_tab_cgroup(&id, pid);
    }
    let config = Config {
        scrolling_history: 10_000,
        ..Config::default()
    };
    let proxy = EventProxy::default();
    let term = Term::new(
        config,
        &TermDims {
            columns: pty_cols,
            screen_lines: pty_rows,
        },
        proxy.clone(),
    );
    let term = Arc::new(FairMutex::new(term));
    // Tap the PTY before alacritty sees it. Every byte goes into the
    // ring first; only then is it forwarded to the parser. The ring
    // survives `\x1b[3J` (alacritty would otherwise wipe its history)
    // and captures Claude / htop / `less` in-place redraws that never
    // reach alacritty's scrollback.
    let pty_ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::default()));
    let pty = crate::pty_ring::PtyTap::new(pty, pty_ring.clone());
    let el = EventLoop::new(term.clone(), proxy.clone(), pty, false, false).ok()?;
    let notifier = el.channel();
    proxy.set_notifier(notifier.clone());
    el.spawn();

    let pending_agent_resume = match (&agent_kind, &agent_session_id) {
        (Some(kind), Some(sid)) => build_agent_resume_command(kind, sid, agent_plan_mode),
        _ => None,
    };

    let last_known_cwd_string = cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    #[cfg(not(feature = "energy"))]
    let _ = energy_wh;

    Some(HeadlessTab {
        id,
        name,
        term,
        notifier,
        event_proxy: proxy,
        pid,
        created_at: Instant::now(),
        prior_uptime: Duration::from_secs_f64(prior_uptime_secs),
        active_duration: Duration::ZERO,
        last_activated: None,
        last_input: None,
        #[cfg(feature = "energy")]
        energy_wh,
        #[cfg(feature = "energy")]
        energy_wh_last_saved: energy_wh,
        output_hash_last_saved: saved_output_hash,
        output_ring_len_last_saved: None,
        pending_restore,
        last_known_cwd: cwd,
        last_known_cwd_string,
        agent_state: None,
        agent_session_id,
        agent_kind,
        agent_plan_mode,
        share_token_rw,
        share_token_ro,
        locked,
        schedule,
        bg_color,
        net_disabled,
        net_allow,
        context: None,
        pending_agent_resume,
        colors_enabled,
        pty_ring,
        snap_cache: None,
        limits: crate::TabResourceLimits::default(),
    })
}

/// Entry point. Drives the headless event loop until SIGINT/SIGTERM
/// asks us to shut down.
///
/// # Errors
/// Returns `io::Error::Other` only when the initial PTY spawn fails for
/// the seed tab — all subsequent failures are logged and the loop
/// keeps running. Returns `Ok(())` on a clean shutdown via SIGTERM.
pub fn run() -> std::io::Result<()> {
    env_logger::init();

    if std::env::args().any(|a| a == "-V" || a == "--version") {
        println!("tab-atelier-headless v{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    info!("starting tab-atelier-headless v{}", env!("CARGO_PKG_VERSION"));

    let prefs = load_preferences(&platform::config_dir());
    // Global default per-tab resource ceilings; each tab's own
    // `limits` overrides per axis. Cloned out before `prefs` is picked
    // apart by the `unwrap_or_else` extractions below.
    let default_limits = prefs.default_tab_limits.clone();

    // Latch the cleared-env opt-in for every tab spawn this process does.
    if prefs.clear_env.unwrap_or(false) {
        crate::CLEAR_ENV.store(true, Ordering::SeqCst);
        crate::set_clear_env_user_vars(prefs.clear_env_vars.clone());
        info!(
            "clear_env: tabs spawn with a cleared environment (minimal allowlist + {} user var(s))",
            prefs.clear_env_vars.len()
        );
    }

    let api_token = api::load_or_generate_token();
    let api_addr = prefs.api_addr.unwrap_or_else(|| DEFAULT_API_ADDR.into());
    let api_tls_addr = prefs.api_tls_addr.unwrap_or_else(|| DEFAULT_API_TLS_ADDR.into());
    // User-supplied TLS cert + key (Cloudflare Origin etc). Both
    // must be set; a half-configured pair falls back to self-signed
    // with a startup warning so the operator notices the misconfig.
    let api_tls_external = match (prefs.api_tls_cert_path, prefs.api_tls_key_path) {
        (Some(c), Some(k)) => Some((std::path::PathBuf::from(c), std::path::PathBuf::from(k))),
        (Some(_), None) | (None, Some(_)) => {
            log::warn!("API/TLS: api_tls_cert_path and api_tls_key_path must both be set; using self-signed");
            None
        }
        (None, None) => None,
    };
    let api_tls_client_ca: Option<std::path::PathBuf> = prefs.api_tls_client_ca_path.map(std::path::PathBuf::from);
    // PTY dims used by every spawn_pty_tab call below. Headless has
    // no display so the dims stay constant for the process lifetime;
    // override via `tab-atelier-headless ports --pty-cols N
    // --pty-rows M`. Clamp to >=4 so a typo can't produce a useless
    // grid.
    let pty_cols = prefs.pty_cols.map_or(INITIAL_COLS, |v| (v as usize).max(4));
    let pty_rows = prefs.pty_rows.map_or(INITIAL_LINES, |v| (v as usize).max(4));
    let global_bg = prefs.tab_bg_color.unwrap_or_else(|| crate::DEFAULT_TAB_BG_COLOR.into());

    let api_url_for_pty = api_url_for_local_clients(&api_addr);

    let read_only = crate::read_only();

    // --- Restore tabs (or seed one fresh tab) ---
    let mut tabs: Vec<HeadlessTab> = Vec::new();
    let mut active: usize = 0;
    let mut windowed = false;

    let saved_state = load_state_with_outputs(&platform::config_base_dir(), &platform::state_base_dir());
    // Set up cgroup delegation once, before any tab spawns, but only
    // when a limit could actually apply (global default set, or some
    // restored tab carries its own) — an unconfigured daemon never
    // touches cgroups. Linux + `Delegate=yes` only; no-op otherwise.
    #[cfg(target_os = "linux")]
    {
        let any_tab_limit = saved_state
            .as_ref()
            .is_some_and(|s| s.tabs.iter().any(|t| !t.limits.is_empty()));
        crate::cgroup::init(!default_limits.is_empty() || any_tab_limit);
    }
    if let Some(saved) = saved_state {
        info!("restoring {} tab(s) from saved state", saved.tabs.len());
        windowed = saved.windowed;
        for ts in &saved.tabs {
            let cwd = ts.cwd.as_ref().map(PathBuf::from);
            let env = tab_env_extras(&ts.id, &api_url_for_pty, &api_token);
            let saved_hash = ts.output.as_deref().map_or(0, |s| crc32(s.as_bytes()));
            // Active tab restores eagerly; others defer until activate
            // (mirrors the GUI cold-launch optimization).
            let is_active = tabs.len() == saved.active;
            let (eager, deferred) = ts.output.clone().map_or((None, None), |out| {
                if is_active {
                    (Some(out), None)
                } else {
                    (None, Some(out))
                }
            });
            if let Some(mut t) = spawn_pty_tab(
                ts.id.clone(),
                ts.name.clone(),
                cwd,
                ts.colors_enabled,
                env,
                ts.uptime_secs.unwrap_or(0.0),
                ts.energy_wh.unwrap_or(0.0),
                saved_hash,
                deferred,
                ts.agent_session_id.clone(),
                ts.agent_kind.clone(),
                ts.agent_plan_mode,
                ts.share_token_rw.clone(),
                ts.share_token_ro.clone(),
                ts.locked,
                ts.schedule.clone(),
                ts.bg_color.clone(),
                pty_cols,
                pty_rows,
                ts.net_disabled,
                ts.allow_config(),
            ) {
                t.limits = ts.limits.clone();
                #[cfg(target_os = "linux")]
                crate::cgroup::apply(
                    &t.id,
                    t.pid,
                    &crate::TabResourceLimits::resolve(&t.limits, &default_limits),
                );
                if let Some(out) = eager {
                    debug!("restoring {} chars of output for '{}'", out.len(), ts.name);
                    t.restore_output(&out);
                }
                tabs.push(t);
            }
        }
        if !tabs.is_empty() {
            active = saved.active.min(tabs.len() - 1);
            tabs[active].activate();
        }
    }

    if tabs.is_empty() {
        let id = default_tab_id();
        let env = tab_env_extras(&id, &api_url_for_pty, &api_token);
        if let Some(mut t) = spawn_pty_tab(
            id,
            "Terminal".into(),
            None,
            true,
            env,
            0.0,
            0.0,
            0,
            None,
            None,
            None,
            None,
            String::new(),
            String::new(),
            false,
            None,
            None,
            pty_cols,
            pty_rows,
            false,
            crate::net_policy::AllowConfig::default(),
        ) {
            // Fresh default tab — no per-tab overrides, so just the
            // global default ceilings.
            #[cfg(target_os = "linux")]
            crate::cgroup::apply(&t.id, t.pid, &default_limits);
            t.activate();
            tabs.push(t);
        }
    }

    if tabs.is_empty() {
        return Err(std::io::Error::other("headless: failed to spawn initial pty"));
    }

    // --- API servers ---
    let api_state = Arc::new(Mutex::new(api::TabSnapshot {
        tabs: Vec::<api::SnapshotTab>::new(),
        master_token: String::new(),
        active,
        #[cfg(feature = "energy")]
        power: Vec::new(),
        #[cfg(feature = "energy")]
        battery_percent: None,
        pending_closes: Vec::new(),
        pending_activate: None,
        pending_input: Vec::new(),
        pending_lock_changes: Vec::new(),
        pending_net_changes: Vec::new(),
        pending_net_allow_changes: Vec::new(),
        pending_bg_color_changes: Vec::new(),
        pending_context_changes: Vec::new(),
        pending_token_rotations: Vec::new(),
        pending_schedule_changes: Vec::new(),
        pending_new_tabs: 0,
        pending_new_tab_cwds: std::collections::VecDeque::new(),
        pending_renames: Vec::new(),
        pending_status_updates: Vec::new(),
        cached_response: None,
    }));
    info!("API server starting on {api_addr} (TLS {api_tls_addr})");
    api::start_api_server(api_state.clone(), api_token.clone(), read_only, api_addr);
    api::start_api_server_tls(
        api_state.clone(),
        api_token.clone(),
        read_only,
        api_tls_addr,
        api_tls_external,
        api_tls_client_ca,
    );

    #[cfg(feature = "energy")]
    let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "energy")]
    let power_watts: Arc<Mutex<Vec<crate::power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "energy")]
    let battery_percent: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));
    #[cfg(feature = "energy")]
    crate::power::start_power_monitor(power_pids.clone(), power_watts.clone(), battery_percent.clone());

    let _ = windowed; // headless doesn't have a window — kept for parity with saved-state shape

    // --- Persist state ---
    let mut last_uptime_save: Option<Instant> = None;
    let mut last_state_hash: u32 = 0;

    // --- Main tick: 100 ms ---
    // Fast 16 ms loop so a keystroke (WS `in` frame or POST /input)
    // lands in the PTY within ~16 ms instead of ~100 ms — the input
    // half of the web-viewer latency. The heavier per-tab work
    // (snapshot rebuild, 2 s disk persist, auto-resume sweep) only
    // needs ~10 Hz, so it's gated to every 6th tick below; the input
    // drain runs every tick.
    let tick_interval = Duration::from_millis(16);
    // Counts fast ticks; the slow block fires every 6th (~96 ms).
    // Input drain is NOT gated by it.
    let mut slow_ctr: u32 = 0;
    // Seed the persist clock 2s in the past so the very first tick
    // forces a flush (state hashing then deduplicates on subsequent
    // ticks). `checked_sub` defensively handles a boot-time clock
    // where `now < 2s` (CI / containers).
    let mut last_persist = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or_else(Instant::now);
    // Heavy per-tab output saves run here, off the main loop, so they
    // can't stall the 16 ms input drain.
    let output_saver = OutputSaver::spawn(platform::state_base_dir());
    loop {
        std::thread::sleep(tick_interval);

        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("graceful shutdown requested by signal, flushing state");
            persist(
                &mut tabs,
                active,
                &api_state,
                #[cfg(feature = "energy")]
                &power_pids,
                #[cfg(feature = "energy")]
                &power_watts,
                #[cfg(feature = "energy")]
                &battery_percent,
                &mut last_uptime_save,
                &mut last_state_hash,
                true,
            );
            for tab in &tabs {
                tab.shutdown();
            }
            return Ok(());
        }

        // Drain pending actions EVERY tick (~16 ms) — this is the
        // latency-critical input path (WS `in` frames / POST /input).
        drain_pending(
            &mut tabs,
            &mut active,
            &api_state,
            &api_token,
            &api_url_for_pty,
            pty_cols,
            pty_rows,
            &default_limits,
        );

        // Everything below is not latency-critical and is gated to
        // ~10 Hz so the fast input loop doesn't pay for grid scans,
        // disk writes, and the resume sweep on every 16 ms tick.
        slow_ctr += 1;
        if slow_ctr < 6 {
            continue;
        }
        slow_ctr = 0;

        // Refresh the API snapshot so /output (HTTP poll path) and the
        // WS meta frame reflect shell echo within ~96 ms. Cheap —
        // grid reads only re-scan a tab whose ring advanced.
        refresh_snapshot(
            &mut tabs,
            active,
            &global_bg,
            &api_state,
            #[cfg(feature = "energy")]
            &power_watts,
            #[cfg(feature = "energy")]
            &battery_percent,
        );

        // Persist on a 2 Hz tick like the GUI's `cx.spawn(timer(2s))`.
        if last_persist.elapsed() >= Duration::from_secs(2) {
            persist(
                &mut tabs,
                active,
                &api_state,
                #[cfg(feature = "energy")]
                &power_pids,
                #[cfg(feature = "energy")]
                &power_watts,
                #[cfg(feature = "energy")]
                &battery_percent,
                &mut last_uptime_save,
                &mut last_state_hash,
                false,
            );
            // Hand the heavy per-tab output save to the background
            // thread (cheap Arc snapshot here); persist() above no
            // longer does it inline except on shutdown.
            if !crate::read_only() {
                output_saver.submit(&tabs);
            }
            last_persist = Instant::now();
        }

        // Auto-resume sweep: type the queued resume command into any
        // tab whose shell has had ~500ms to print its prompt.
        for tab in &mut tabs {
            if tab.pending_agent_resume.is_some() && tab.created_at.elapsed().as_millis() >= 500 {
                tab.flush_pending_agent_resume();
            }
        }
    }
}

/// Rebuild the API snapshot from the runtime tab state and write it
/// into `api_state`. Called every drain tick (not just every 2 s
/// `persist`) so /output reflects keystroke echoes within ~tick
/// duration instead of waiting for the next disk-persist cycle.
#[allow(clippy::too_many_arguments)]
fn refresh_snapshot(
    tabs: &mut [HeadlessTab],
    active: usize,
    global_bg: &str,
    api_state: &Arc<Mutex<api::TabSnapshot>>,
    #[cfg(feature = "energy")] power_watts: &Arc<Mutex<Vec<crate::power::TabPower>>>,
    #[cfg(feature = "energy")] battery_percent: &Arc<Mutex<Option<u8>>>,
) {
    let mut api_tabs: Vec<api::SnapshotTab> = Vec::with_capacity(tabs.len());
    for tab in tabs.iter_mut() {
        // Grid-derived fields come from the per-tab cache, which only
        // re-scans the terminal when the PTY ring advanced. The other
        // fields (uptime, lock, agent state, …) are cheap and rebuilt
        // every tick so changes there still surface immediately.
        let grid = tab.cached_grid().clone();
        api_tabs.push(api::SnapshotTab {
            id: tab.id.clone(),
            name: tab.name.clone(),
            cwd: tab.last_known_cwd_string.clone(),
            output: grid.output,
            raw_output: grid.raw_output,
            raw_cursor: grid.raw_cursor,
            uptime_secs: tab.uptime().as_secs_f64(),
            cursor: grid.cursor,
            cols: grid.cols,
            rows: grid.rows,
            share_token_rw: tab.share_token_rw.clone(),
            share_token_ro: tab.share_token_ro.clone(),
            locked: tab.locked,
            schedule: tab.schedule.clone(),
            bg_color: crate::effective_tab_bg(tab.bg_color.as_deref(), Some(global_bg)).to_string(),
            context: tab.context.clone(),
            shell_pid: tab.pid,
            agent_state: tab.agent_state.clone(),
            agent_session_id: tab.agent_session_id.clone(),
            agent_kind: tab.agent_kind.clone(),
            viewers: tab.pty_ring.lock().map_or(0, |r| r.viewer_count()),
            pty_ring: Some(tab.pty_ring.clone()),
            net_disabled: tab.net_disabled,
        });
    }
    let mut snapshot = api_state.lock().unwrap();
    snapshot.tabs = api_tabs;
    snapshot.active = active;
    snapshot.cached_response = None;
    #[cfg(feature = "energy")]
    snapshot.power.clone_from(&power_watts.lock().unwrap());
    #[cfg(feature = "energy")]
    {
        snapshot.battery_percent = *battery_percent.lock().unwrap();
    }
}

// Too many parameters because cfg-gated energy features triple the
// arg count. Easier than packaging them into a Context struct that
// only adds plumbing.
/// One tab's output-save job handed to the background saver thread.
struct SaveJob {
    name: String,
    term: Arc<FairMutex<Term<EventProxy>>>,
    ring_len: u64,
}

/// Background thread that runs the expensive `copy_all_history`
/// (scrollback → ANSI string) + atomic disk write OFF the main loop.
///
/// The periodic 2 s output-save used to run inline on the single main
/// thread, so a flood of active tabs froze the input drain that shares
/// that thread for up to ~1.5 s (the p99 keystroke stall). Now the main
/// loop only submits cheap `Arc` handles; this thread reconstructs each
/// tab's scrollback and writes it. `Term`/ring are behind `FairMutex`,
/// and `save_tab_output` writes atomically (temp + rename), so the only
/// cross-thread contention is a brief lock while reading the grid.
struct OutputSaver {
    tx: std::sync::mpsc::Sender<Vec<SaveJob>>,
}

impl OutputSaver {
    fn spawn(state_base: std::path::PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<SaveJob>>();
        std::thread::Builder::new()
            .name("ta-output-saver".into())
            .spawn(move || {
                // Per-tab dirtiness gate (ring_len, output crc) — the
                // same logic the inline loop used, just kept here now.
                let mut seen: HashMap<String, (u64, u32)> = HashMap::new();
                while let Ok(mut batch) = rx.recv() {
                    // Saves are current-state + idempotent, so if newer
                    // batches queued while we worked, jump to the latest.
                    while let Ok(newer) = rx.try_recv() {
                        batch = newer;
                    }
                    for job in batch {
                        if seen.get(&job.name).is_some_and(|&(rl, _)| rl == job.ring_len) {
                            continue; // ring unchanged ⇒ identical output
                        }
                        let output = crate::term_export::term_to_ansi_text_with_cursor(&job.term, None).0;
                        if output.is_empty() {
                            continue;
                        }
                        let h = crate::crc32(output.as_bytes());
                        if seen.get(&job.name).is_some_and(|&(_, hh)| hh == h) {
                            seen.insert(job.name, (job.ring_len, h));
                            continue;
                        }
                        save_tab_output(&state_base, &job.name, &output);
                        seen.insert(job.name, (job.ring_len, h));
                    }
                }
            })
            .expect("spawn output-saver thread");
        Self { tx }
    }

    /// Main-thread side: snapshot `Arc` handles + ring lengths and hand
    /// them off. Cheap (Arc clone + a brief ring lock per tab) — never
    /// blocks on the scrollback copy or the disk write.
    fn submit(&self, tabs: &[HeadlessTab]) {
        let batch: Vec<SaveJob> = tabs
            .iter()
            .map(|t| SaveJob {
                name: t.name.clone(),
                term: t.term.clone(),
                ring_len: t.ring_total_len(),
            })
            .collect();
        let _ = self.tx.send(batch); // ignore if the saver has exited
    }
}

#[allow(clippy::too_many_arguments)]
fn persist(
    tabs: &mut [HeadlessTab],
    active: usize,
    // Snapshot writeback moved to refresh_snapshot; this is kept on
    // the signature for forward compat (callers shouldn't have to
    // change). _-prefixed to silence unused-warning.
    _api_state: &Arc<Mutex<api::TabSnapshot>>,
    #[cfg(feature = "energy")] power_pids: &Arc<Mutex<Vec<u32>>>,
    #[cfg(feature = "energy")] power_watts: &Arc<Mutex<Vec<crate::power::TabPower>>>,
    #[cfg(feature = "energy")] _battery_percent: &Arc<Mutex<Option<u8>>>,
    last_uptime_save: &mut Option<Instant>,
    last_state_hash: &mut u32,
    final_flush: bool,
) {
    let read_only = crate::read_only();
    let state_base = platform::state_base_dir();

    // Activate/deactivate the active tab based on input recency, same
    // 30s idle threshold the GUI uses.
    if active < tabs.len() {
        let tab = &mut tabs[active];
        let idle = tab.last_input.is_none_or(|t| t.elapsed().as_secs() >= 30);
        if idle && tab.last_activated.is_some() {
            tab.deactivate();
        } else if !idle && tab.last_activated.is_none() {
            tab.activate();
        }
    }

    #[cfg(feature = "energy")]
    {
        let watts = power_watts.lock().unwrap();
        for (i, tab) in tabs.iter_mut().enumerate() {
            if let Some(w) = watts.get(i).and_then(|p| p.watts) {
                tab.energy_wh += w * 2.0 / 3600.0;
            }
        }
    }

    // Refresh last_known_cwd for any live tab; sticky on shell exit.
    for tab in tabs.iter_mut() {
        if let Some(p) = platform::process_cwd(tab.pid)
            && tab.last_known_cwd.as_deref() != Some(p.as_path())
        {
            tab.last_known_cwd_string = Some(p.to_string_lossy().into_owned());
            tab.last_known_cwd = Some(p);
        }
    }

    let tab_states: Vec<TabState> = tabs
        .iter()
        .map(|tab| TabState {
            id: tab.id.clone(),
            name: tab.name.clone(),
            cwd: tab.last_known_cwd_string.clone(),
            colors_enabled: tab.colors_enabled,
            agent_session_id: tab.agent_session_id.clone(),
            agent_kind: tab.agent_kind.clone(),
            agent_plan_mode: tab.agent_plan_mode,
            share_token_rw: tab.share_token_rw.clone(),
            share_token_ro: tab.share_token_ro.clone(),
            locked: tab.locked,
            net_disabled: tab.net_disabled,
            net_allow_presets: tab.net_allow.presets.clone(),
            net_allow_domains: tab.net_allow.domains.clone(),
            net_allow_cidrs: tab.net_allow.cidrs.clone(),
            schedule: tab.schedule.clone(),
            bg_color: tab.bg_color.clone(),
            limits: tab.limits.clone(),
            ..TabState::default()
        })
        .collect();

    // (Snapshot rebuild moved to `refresh_snapshot` which runs every
    // tick — persist() now only does disk I/O.)

    let saved = SavedState {
        tabs: tab_states,
        active,
        windowed: false,
    };
    let serialized = serde_json::to_string_pretty(&saved).unwrap_or_default();
    let new_hash = crc32(serialized.as_bytes());
    if !read_only && (final_flush || new_hash != *last_state_hash) {
        save_state(&platform::config_base_dir(), &saved);
        *last_state_hash = new_hash;
    }

    // Periodic output saves run on the background `OutputSaver` thread
    // (submitted from the main loop) so they never freeze the input
    // drain. Only the shutdown `final_flush` saves inline here, where a
    // synchronous write before exit is exactly what we want.
    if !read_only && final_flush {
        for tab in tabs.iter_mut() {
            // Dirtiness gate before the expensive copy_all_history():
            // skip tabs whose PTY ring hasn't advanced since the last
            // save (no new grid bytes ⇒ identical output). The final
            // flush ignores the gate so shutdown always persists the
            // latest. crc32 below stays authoritative on every real
            // change.
            let ring_len = tab.ring_total_len();
            if !final_flush && tab.output_ring_len_last_saved == Some(ring_len) {
                continue;
            }
            let output = tab.copy_all_history();
            if output.is_empty() {
                continue;
            }
            let h = crc32(output.as_bytes());
            if !final_flush && h == tab.output_hash_last_saved {
                tab.output_ring_len_last_saved = Some(ring_len);
                continue;
            }
            save_tab_output(&state_base, &tab.name, &output);
            tab.output_hash_last_saved = h;
            tab.output_ring_len_last_saved = Some(ring_len);
        }
    }

    if !read_only {
        let should_save_uptime = final_flush || last_uptime_save.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
        if should_save_uptime {
            for tab in tabs.iter() {
                save_tab_uptime(&state_base, &tab.name, tab.uptime().as_secs_f64());
            }
            *last_uptime_save = Some(Instant::now());
        }
        #[cfg(feature = "energy")]
        {
            const ENERGY_DELTA_WH: f64 = 0.1;
            for tab in tabs.iter_mut() {
                if final_flush || (tab.energy_wh - tab.energy_wh_last_saved).abs() >= ENERGY_DELTA_WH {
                    save_tab_energy(&state_base, &tab.name, tab.energy_wh);
                    tab.energy_wh_last_saved = tab.energy_wh;
                }
            }
        }
        #[cfg(feature = "catbus")]
        for tab in tabs.iter() {
            if let Some(session) = crate::catbus_agent::find_session(tab.pid)
                && let Some(usage) = crate::catbus_agent::read_session_tokens(&session)
            {
                crate::save_tab_tokens(&state_base, &tab.name, &usage);
            }
        }
    }

    // Snapshot is owned by `refresh_snapshot` now — nothing to write
    // back here. Power PIDs still tracked for the energy feature.
    #[cfg(feature = "energy")]
    {
        let pids: Vec<u32> = tabs.iter().map(|tab| tab.pid).collect();
        *power_pids.lock().unwrap() = pids;
    }
}

/// Rebuild a tab's PTY with a new network state (off flag + allowlist
/// config). There's no in-place re-jail / re-proxy, so the shell restarts;
/// the current scrollback is fed back via `pending_restore` and durable
/// fields carry across. Used by both the net on/off and allowlist drains.
#[allow(clippy::too_many_arguments)]
fn respawn_tab_net(
    tabs: &mut [HeadlessTab],
    idx: usize,
    active: usize,
    net_disabled: bool,
    net_allow: crate::net_policy::AllowConfig,
    api_url_for_pty: &str,
    api_token: &str,
    pty_cols: usize,
    pty_rows: usize,
    default_limits: &crate::TabResourceLimits,
) {
    let was_active = active == idx;
    let cwd = platform::process_cwd(tabs[idx].pid).or_else(|| tabs[idx].last_known_cwd.clone());
    let history = tabs[idx].copy_all_history();
    let pending_restore = if history.is_empty() { None } else { Some(history) };
    let env = tab_env_extras(&tabs[idx].id, api_url_for_pty, api_token);
    let id = tabs[idx].id.clone();
    let name = tabs[idx].name.clone();
    let prior = tabs[idx].uptime().as_secs_f64();
    #[cfg(feature = "energy")]
    let energy = tabs[idx].energy_wh;
    #[cfg(not(feature = "energy"))]
    let energy = 0.0;
    let saved_hash = tabs[idx].output_hash_last_saved;
    let agent_session_id = tabs[idx].agent_session_id.clone();
    let agent_kind = tabs[idx].agent_kind.clone();
    let agent_plan_mode = tabs[idx].agent_plan_mode;
    let rw = tabs[idx].share_token_rw.clone();
    let ro = tabs[idx].share_token_ro.clone();
    let locked = tabs[idx].locked;
    let schedule = tabs[idx].schedule.clone();
    let bg = tabs[idx].bg_color.clone();
    tabs[idx].shutdown();
    if let Some(mut t) = spawn_pty_tab(
        id,
        name,
        cwd,
        true,
        env,
        prior,
        energy,
        saved_hash,
        pending_restore,
        agent_session_id,
        agent_kind,
        agent_plan_mode,
        rw,
        ro,
        locked,
        schedule,
        bg,
        pty_cols,
        pty_rows,
        net_disabled,
        net_allow,
    ) {
        #[cfg(target_os = "linux")]
        crate::cgroup::apply(&t.id, t.pid, default_limits);
        #[cfg(not(target_os = "linux"))]
        let _ = default_limits;
        if was_active {
            t.activate();
            t.flush_pending_restore();
        }
        tabs[idx] = t;
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_pending(
    tabs: &mut Vec<HeadlessTab>,
    active: &mut usize,
    api_state: &Arc<Mutex<api::TabSnapshot>>,
    api_token: &str,
    api_url_for_pty: &str,
    pty_cols: usize,
    pty_rows: usize,
    default_limits: &crate::TabResourceLimits,
) {
    let mut s = api_state.lock().unwrap();
    let mut closes: Vec<usize> = s.pending_closes.drain(..).collect();
    let activate = s.pending_activate.take();
    let inputs: Vec<(usize, Vec<u8>)> = s.pending_input.drain(..).collect();
    let renames: Vec<(usize, String)> = s.pending_renames.drain(..).collect();
    let status_updates: Vec<api::PendingStatusUpdate> = s.pending_status_updates.drain(..).collect();
    let lock_changes: Vec<(String, bool)> = s.pending_lock_changes.drain(..).collect();
    let net_changes: Vec<(String, bool)> = s.pending_net_changes.drain(..).collect();
    let net_allow_changes: Vec<(String, crate::net_policy::AllowConfig)> =
        s.pending_net_allow_changes.drain(..).collect();
    let bg_color_changes: Vec<(String, Option<String>)> = s.pending_bg_color_changes.drain(..).collect();
    let context_changes: Vec<(String, Option<String>)> = s.pending_context_changes.drain(..).collect();
    let token_rotations: Vec<String> = s.pending_token_rotations.drain(..).collect();
    let schedule_changes: Vec<(String, Option<crate::schedule::TabSchedule>)> =
        s.pending_schedule_changes.drain(..).collect();
    let new_tabs = std::mem::take(&mut s.pending_new_tabs);
    let new_tab_cwds: std::collections::VecDeque<std::path::PathBuf> = std::mem::take(&mut s.pending_new_tab_cwds);
    drop(s);
    // CLI / API lock toggles → runtime HeadlessTab. tabs.json picks
    // it up on the same persist tick a few lines below.
    for (tab_id, locked) in lock_changes {
        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
            t.locked = locked;
        }
    }
    // Schedule changes — None clears, Some sets.
    for (tab_id, sched) in schedule_changes {
        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
            t.schedule = sched;
        }
    }
    // CLI / API net toggles → rebuild the tab's PTY with the new
    // bubblewrap netns state. There's no in-place re-jail, so the shell
    // restarts; the current scrollback is fed back via pending_restore so
    // the screen isn't blanked. Durable fields (id/name/tokens/schedule/
    // agent/uptime) carry across. The bwrap-availability guard lives at
    // the endpoint, so a queued change is already known applicable.
    for (tab_id, disabled) in net_changes {
        if let Some(idx) = tabs.iter().position(|t| t.id == tab_id) {
            let allow = tabs[idx].net_allow.clone();
            respawn_tab_net(
                tabs,
                idx,
                *active,
                disabled,
                allow,
                api_url_for_pty,
                api_token,
                pty_cols,
                pty_rows,
                default_limits,
            );
        }
    }
    // Allowlist set/clear: a non-empty config implies net-on (clears the
    // airgap); an empty config clears allowlist mode, keeping whatever
    // net_disabled the tab already had.
    for (tab_id, config) in net_allow_changes {
        if let Some(idx) = tabs.iter().position(|t| t.id == tab_id) {
            let disabled = if config.is_empty() {
                tabs[idx].net_disabled
            } else {
                false
            };
            respawn_tab_net(
                tabs,
                idx,
                *active,
                disabled,
                config,
                api_url_for_pty,
                api_token,
                pty_cols,
                pty_rows,
                default_limits,
            );
        }
    }
    // Revoke per-tab share tokens (the snapshot was already cleared by
    // the endpoint); persists the cleared state into tabs.json below.
    for tab_id in token_rotations {
        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
            t.share_token_rw.clear();
            t.share_token_ro.clear();
        }
    }
    // Same path for the bg-color override.
    for (tab_id, color) in bg_color_changes {
        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
            t.bg_color = color;
        }
    }
    // …and the per-tab agent context.
    for (tab_id, context) in context_changes {
        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
            t.context = context;
        }
    }

    // Status updates: write transient + durable agent fields.
    for upd in status_updates {
        let Some(tab) = tabs.iter_mut().find(|t| t.id == upd.tab_id) else {
            continue;
        };
        if upd.label.as_deref() == Some("__clear__") {
            tab.agent_state = None;
            tab.agent_session_id = None;
            tab.agent_kind = None;
            tab.agent_plan_mode = None;
        } else {
            tab.agent_state = Some(AgentStateSnapshot {
                state: upd.state,
                label: upd.label,
                updated_at: Instant::now(),
            });
            if upd.session_id.is_some() {
                tab.agent_session_id = upd.session_id;
            }
            if upd.agent_kind.is_some() {
                tab.agent_kind = upd.agent_kind;
            }
            if upd.plan_mode.is_some() {
                tab.agent_plan_mode = upd.plan_mode;
            }
        }
    }

    // Working-subprocess sweep — same logic as the GUI tick.
    let now = Instant::now();
    #[cfg(feature = "catbus")]
    for tab in tabs.iter_mut() {
        if tab.agent_kind.is_none() {
            continue;
        }
        if !crate::catbus_agent::agent_has_active_subprocess(tab.pid) {
            continue;
        }
        tab.agent_state = Some(match tab.agent_state.take() {
            Some(mut snap) if snap.state != crate::AgentState::Error => {
                snap.state = crate::AgentState::Thinking;
                snap.updated_at = now;
                snap
            }
            Some(snap) => snap,
            None => AgentStateSnapshot {
                state: crate::AgentState::Thinking,
                label: Some("subproc".into()),
                updated_at: now,
            },
        });
    }

    // Staleness sweep: drop transient state older than 2 min.
    for tab in tabs.iter_mut() {
        if let Some(snap) = &tab.agent_state
            && now.duration_since(snap.updated_at).as_secs() > 120
        {
            tab.agent_state = None;
        }
    }

    // Process-presence sweep: clear agent attachment when CLI is gone.
    #[cfg(feature = "catbus")]
    for tab in tabs.iter_mut() {
        if tab.agent_kind.is_some() && !crate::catbus_agent::has_agent_descendant(tab.pid) {
            tab.agent_state = None;
            tab.agent_session_id = None;
            tab.agent_kind = None;
            tab.agent_plan_mode = None;
        }
    }

    // Renames (with file-side renames of per-tab output / uptime / power).
    for (idx, new_name) in renames {
        if idx >= tabs.len() {
            continue;
        }
        let old_name = tabs[idx].name.clone();
        if old_name == new_name {
            continue;
        }
        if !crate::read_only() {
            let base = platform::state_base_dir();
            for resolver in [
                crate::tab_output_path as fn(&std::path::Path, &str) -> std::path::PathBuf,
                crate::tab_uptime_path,
                crate::tab_power_path,
            ] {
                let old_path = resolver(&base, &old_name);
                let new_path = resolver(&base, &new_name);
                if old_path.exists() {
                    let _ = std::fs::rename(&old_path, &new_path);
                    let _ = std::fs::rename(old_path.with_extension("json.bak"), new_path.with_extension("json.bak"));
                }
            }
        }
        tabs[idx].name = new_name;
    }

    // Closes (highest index first).
    closes.sort_unstable();
    closes.dedup();
    for idx in closes.into_iter().rev() {
        if idx < tabs.len() && tabs.len() > 1 {
            let was_active = *active == idx;
            tabs[idx].deactivate();
            tabs[idx].shutdown();
            // Drop any kernel egress allowlist for the closed tab (no-op
            // unless enforcement was opted in and applied).
            #[cfg(target_os = "linux")]
            crate::net_nft::teardown(&tabs[idx].id);
            tabs.remove(idx);
            if *active >= tabs.len() {
                *active = tabs.len() - 1;
            } else if *active > idx {
                *active -= 1;
            }
            if was_active && *active < tabs.len() {
                tabs[*active].activate();
                tabs[*active].flush_pending_restore();
            }
        }
    }

    // Activate.
    if let Some(idx) = activate
        && idx < tabs.len()
        && *active != idx
    {
        tabs[*active].deactivate();
        *active = idx;
        tabs[idx].activate();
        tabs[idx].flush_pending_restore();
    }

    // Input.
    for (idx, bytes) in inputs {
        if idx < tabs.len() {
            tabs[idx].send_input_bytes(bytes);
        }
    }

    // New tabs from the API.
    let mut cwd_hint_iter = new_tab_cwds.into_iter();
    for _ in 0..new_tabs {
        let cwd = cwd_hint_iter.next().filter(|p| p.is_dir()).or_else(|| {
            if *active < tabs.len() {
                platform::process_cwd(tabs[*active].pid).or_else(|| tabs[*active].last_known_cwd.clone())
            } else {
                None
            }
        });
        let id = default_tab_id();
        let env = tab_env_extras(&id, api_url_for_pty, api_token);
        let name = format!("Terminal {}", tabs.len());
        if let Some(mut t) = spawn_pty_tab(
            id,
            name,
            cwd,
            true,
            env,
            0.0,
            0.0,
            0,
            None,
            None,
            None,
            None,
            String::new(),
            String::new(),
            false,
            None,
            None,
            pty_cols,
            pty_rows,
            false,
            crate::net_policy::AllowConfig::default(),
        ) {
            // API-created tab — global default ceilings (no per-tab
            // overrides exist until one is set).
            #[cfg(target_os = "linux")]
            crate::cgroup::apply(&t.id, t.pid, default_limits);
            #[cfg(not(target_os = "linux"))]
            let _ = default_limits;
            if *active < tabs.len() {
                tabs[*active].deactivate();
            }
            t.activate();
            tabs.push(t);
            *active = tabs.len() - 1;
        }
    }
}
