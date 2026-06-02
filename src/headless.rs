// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless tab-atelier entry point.
//!
//! Restores every tab from `tabs.json`, spawns its PTY through
//! `alacritty_terminal::tty`, runs the same local HTTP / TLS API and
//! happier-bridge spawn the desktop GUI uses, and persists output /
//! uptime / energy / token state on a 2 Hz tick. No display server, no
//! gpui, no x11rb — just libc + alacritty + rustls.
//!
//! Drains the same pending-action queues the GUI's `persist()` does
//! (closes / activate / input / rename / status updates / new-tab
//! requests) so anything that talks to `/tabs/*` keeps working
//! identically against this binary.

#![cfg(not(feature = "gui"))]

#[cfg(feature = "happier-bridge")]
use crate::happier_relay_url_from_args;
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
    AgentStateSnapshot, DEFAULT_API_ADDR, DEFAULT_API_TLS_ADDR, DEFAULT_HAPPIER_RELAY_ADDR, SHUTDOWN_REQUESTED,
    SavedState, TabState, crc32, default_tab_id, load_preferences, load_state_with_outputs, save_state,
    save_tab_output, save_tab_uptime,
};

const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;

// Shared with the GUI — see `crate::tab_env_extras`,
// `crate::api_url_for_local_clients`, `crate::build_agent_resume_command`,
// and `crate::happier_relay_url_from_args` in lib.rs.

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
    pending_restore: Option<String>,
    last_known_cwd: Option<PathBuf>,
    last_known_cwd_string: Option<String>,
    agent_state: Option<AgentStateSnapshot>,
    agent_session_id: Option<String>,
    agent_kind: Option<String>,
    agent_plan_mode: Option<bool>,
    pending_agent_resume: Option<String>,
    colors_enabled: bool,
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

    fn dims(&self) -> (u16, u16) {
        let t = self.term.lock();
        let g = t.grid();
        (g.columns() as u16, g.screen_lines() as u16)
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
) -> Option<HeadlessTab> {
    let ws = WindowSize {
        num_lines: INITIAL_LINES as u16,
        num_cols: INITIAL_COLS as u16,
        cell_width: 9,
        cell_height: 18,
    };
    let mut env = pty_env(colors_enabled);
    env.extend(extra_env);
    let opts = tty::Options {
        working_directory: cwd.clone(),
        env,
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
    let config = Config {
        scrolling_history: 10_000,
        ..Config::default()
    };
    let proxy = EventProxy::default();
    let term = Term::new(
        config,
        &TermDims {
            columns: INITIAL_COLS,
            screen_lines: INITIAL_LINES,
        },
        proxy.clone(),
    );
    let term = Arc::new(FairMutex::new(term));
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
        pending_restore,
        last_known_cwd: cwd,
        last_known_cwd_string,
        agent_state: None,
        agent_session_id,
        agent_kind,
        agent_plan_mode,
        pending_agent_resume,
        colors_enabled,
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

    let api_token = api::load_or_generate_token();
    let api_addr = prefs.api_addr.unwrap_or_else(|| DEFAULT_API_ADDR.into());
    let api_tls_addr = prefs.api_tls_addr.unwrap_or_else(|| DEFAULT_API_TLS_ADDR.into());
    #[cfg_attr(not(feature = "happier-bridge"), allow(unused_variables))]
    let happier_relay_addr = prefs
        .happier_relay_addr
        .unwrap_or_else(|| DEFAULT_HAPPIER_RELAY_ADDR.into());

    let api_url_for_pty = api_url_for_local_clients(&api_addr);

    let read_only = crate::read_only();

    // --- Restore tabs (or seed one fresh tab) ---
    let mut tabs: Vec<HeadlessTab> = Vec::new();
    let mut active: usize = 0;
    let mut windowed = false;

    if let Some(saved) = load_state_with_outputs(&platform::config_base_dir(), &platform::state_base_dir()) {
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
            if let Some(t) = spawn_pty_tab(
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
            ) {
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
        ) {
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
        active,
        #[cfg(feature = "energy")]
        power: Vec::new(),
        #[cfg(feature = "energy")]
        battery_percent: None,
        pending_closes: Vec::new(),
        pending_activate: None,
        pending_input: Vec::new(),
        pending_new_tabs: 0,
        pending_new_tab_cwds: std::collections::VecDeque::new(),
        pending_renames: Vec::new(),
        pending_status_updates: Vec::new(),
        cached_response: None,
    }));
    info!("API server starting on {api_addr} (TLS {api_tls_addr})");
    api::start_api_server(api_state.clone(), api_token.clone(), read_only, api_addr);
    api::start_api_server_tls(api_state.clone(), api_token.clone(), read_only, api_tls_addr);

    #[cfg(feature = "energy")]
    let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "energy")]
    let power_watts: Arc<Mutex<Vec<crate::power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "energy")]
    let battery_percent: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));
    #[cfg(feature = "energy")]
    crate::power::start_power_monitor(power_pids.clone(), power_watts.clone(), battery_percent.clone());

    // --- happier-bridge spawn (auto-relay + optional publisher) ---
    #[cfg(feature = "happier-bridge")]
    let _relay_handle = match crate::happier_bridge::spawn_relay(&happier_relay_addr) {
        Ok(handle) => {
            info!(
                "happier-relay spawned at https://{happier_relay_addr} (pid {})",
                handle.pid()
            );
            Some(handle)
        }
        Err(e) => {
            warn!("happier-relay not spawned: {e}");
            None
        }
    };

    #[cfg(feature = "happier-bridge")]
    {
        if let Some(url) = happier_relay_url_from_args() {
            crate::happier_bridge::spawn(url, api_state.clone());
        } else {
            crate::happier_bridge::spawn(format!("https://{happier_relay_addr}"), api_state.clone());
        }
    }

    let _ = windowed; // headless doesn't have a window — kept for parity with saved-state shape

    // --- Persist state ---
    let mut last_uptime_save: Option<Instant> = None;
    let mut last_state_hash: u32 = 0;

    // --- Main tick: 500ms, mirrors the GUI persist + tick fan-out ---
    let tick_interval = Duration::from_millis(500);
    // Seed the persist clock 2s in the past so the very first tick
    // forces a flush (state hashing then deduplicates on subsequent
    // ticks). `checked_sub` defensively handles a boot-time clock
    // where `now < 2s` (CI / containers).
    let mut last_persist = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or_else(Instant::now);
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

        // Drain pending actions every tick.
        drain_pending(&mut tabs, &mut active, &api_state, &api_token, &api_url_for_pty);

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

#[allow(clippy::too_many_arguments)]
fn persist(
    tabs: &mut [HeadlessTab],
    active: usize,
    api_state: &Arc<Mutex<api::TabSnapshot>>,
    #[cfg(feature = "energy")] power_pids: &Arc<Mutex<Vec<u32>>>,
    #[cfg(feature = "energy")] power_watts: &Arc<Mutex<Vec<crate::power::TabPower>>>,
    #[cfg(feature = "energy")] battery_percent: &Arc<Mutex<Option<u8>>>,
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
            ..TabState::default()
        })
        .collect();

    let api_tabs: Vec<api::SnapshotTab> = tabs
        .iter()
        .zip(tab_states.iter())
        .map(|(tab, ts)| {
            let (output, cursor) = tab.ansi_text_with_cursor(Some(200));
            let (cols, rows) = tab.dims();
            api::SnapshotTab {
                id: tab.id.clone(),
                name: ts.name.clone(),
                cwd: ts.cwd.clone(),
                output,
                uptime_secs: tab.uptime().as_secs_f64(),
                cursor,
                cols,
                rows,
                shell_pid: tab.pid,
                agent_state: tab.agent_state.clone(),
                agent_session_id: tab.agent_session_id.clone(),
                agent_kind: tab.agent_kind.clone(),
            }
        })
        .collect();

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

    if !read_only {
        for tab in tabs.iter_mut() {
            let output = tab.copy_all_history();
            if output.is_empty() {
                continue;
            }
            let h = crc32(output.as_bytes());
            if !final_flush && h == tab.output_hash_last_saved {
                continue;
            }
            save_tab_output(&state_base, &tab.name, &output);
            tab.output_hash_last_saved = h;
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

    {
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

    #[cfg(feature = "energy")]
    {
        let pids: Vec<u32> = tabs.iter().map(|tab| tab.pid).collect();
        *power_pids.lock().unwrap() = pids;
    }
}

fn drain_pending(
    tabs: &mut Vec<HeadlessTab>,
    active: &mut usize,
    api_state: &Arc<Mutex<api::TabSnapshot>>,
    api_token: &str,
    api_url_for_pty: &str,
) {
    let mut s = api_state.lock().unwrap();
    let mut closes: Vec<usize> = s.pending_closes.drain(..).collect();
    let activate = s.pending_activate.take();
    let inputs: Vec<(usize, Vec<u8>)> = s.pending_input.drain(..).collect();
    let renames: Vec<(usize, String)> = s.pending_renames.drain(..).collect();
    let status_updates: Vec<api::PendingStatusUpdate> = s.pending_status_updates.drain(..).collect();
    let new_tabs = std::mem::take(&mut s.pending_new_tabs);
    let new_tab_cwds: std::collections::VecDeque<std::path::PathBuf> = std::mem::take(&mut s.pending_new_tab_cwds);
    drop(s);

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
        if let Some(mut t) = spawn_pty_tab(id, name, cwd, true, env, 0.0, 0.0, 0, None, None, None, None) {
            if *active < tabs.len() {
                tabs[*active].deactivate();
            }
            t.activate();
            tabs.push(t);
            *active = tabs.len() - 1;
        }
    }
}
