// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "gui")]

use crate::api;
use crate::locale::{self, Lang, Strings};
use crate::platform;
#[cfg(feature = "energy")]
use crate::power;
use crate::screenshot;
use crate::terminal::TerminalView;
use crate::theme::{self, ThemeName};
use crate::tracking::WakatimeTracker;
use crate::{
    DEFAULT_HOTKEYS, FontConfig, Preferences, SavedState, TabState, gpui_key_to_keycode, keycode_label,
    load_preferences, load_state_with_outputs, load_wakatime_key, resolve_font_config, save_preferences, save_state,
    save_tab_output, save_tab_uptime,
};
// Feature-gated extras: clippy --features gui flagged these as
// "unused imports" because the cfg(feature = "energy"/"catbus")
// call sites don't compile in that profile; but the default-features
// build (CI) does need them.
#[cfg(feature = "energy")]
use crate::save_tab_energy;
#[cfg(feature = "catbus")]
use crate::save_tab_tokens;
use crate::{api_url_for_local_clients, build_agent_resume_command, tab_env_extras};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Application, AsyncApp, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowHandle, WindowOptions, div, px, rgba,
};
use log::{debug, info};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// How long an attached-but-quiet tab must go un-focused before its LED
/// turns blue ("dormant"). The transient state is already swept to None
/// after 2 min of agent silence, so this is the extra "and you haven't
/// looked at it" grace on top of that.
const DORMANT_AFTER_SECS: u64 = 180;

struct Tab {
    view: Entity<TerminalView>,
    name: String,
    /// Wall-clock instant we started this tab in *this* process run.
    /// Persisted uptime is folded in via `prior_uptime` so a restart
    /// doesn't reset the counter to zero.
    created_at: std::time::Instant,
    /// Uptime accumulated in previous process runs, loaded from
    /// `tab-<name>.uptime.json`. Added to `created_at.elapsed()` in
    /// `Tab::uptime()`.
    prior_uptime: std::time::Duration,
    active_duration: std::time::Duration,
    last_activated: Option<std::time::Instant>,
    /// When this tab was last the foreground (focused) tab. Refreshed
    /// every persist tick for the active tab; ages for the rest. Drives
    /// the "dormant" (blue) LED — an attached-but-quiet session you
    /// haven't opened in a while. `None` = never focused this run.
    last_focused_at: Option<std::time::Instant>,
    #[cfg(feature = "energy")]
    energy_wh: f64,
    /// Last `energy_wh` value flushed to disk. Used to skip writes when no
    /// meaningful additional energy has been consumed since last save.
    #[cfg(feature = "energy")]
    energy_wh_last_saved: f64,
    /// CRC32 of the last output snapshot written to disk. Skips the
    /// per-tab `output_tab-...json` write+rotate when nothing changed
    /// (idle tabs, no new output since last persist tick).
    output_hash_last_saved: u32,
    /// PTY-ring `total_len` at the last output save. The crc32 gate
    /// above is authoritative for "did the output change", but
    /// computing the hash needs `copy_all_history()` — a full
    /// scrollback serialization — which dominated idle CPU with many
    /// tabs (49 tabs × full-grid scan every 2 s). When the ring's
    /// monotonic byte counter hasn't moved since the last save, no
    /// new PTY bytes reached the grid, so the output is byte-identical
    /// and we skip the serialize+hash entirely. `None` until the
    /// first save so the very first persist always serializes.
    output_ring_len_last_saved: Option<u64>,
    /// Saved scrollback that hasn't been fed back into the terminal yet.
    /// Tabs other than the active one defer this work until first focus
    /// so cold-launch with many tabs doesn't block on vte-parsing each
    /// one's entire history up front.
    pending_restore: Option<String>,
    /// Last cwd we successfully read from /proc/PID/cwd for this tab's
    /// shell. Used as a sticky fallback so that a dead or exited shell
    /// doesn't blank out the persisted cwd on the next tick.
    last_known_cwd: Option<PathBuf>,
    /// String form of `last_known_cwd`. Held alongside the `PathBuf` so the
    /// 2 s persist tick doesn't redo `to_string_lossy` for every tab on
    /// every tick — most ticks see no cwd change at all.
    last_known_cwd_string: Option<String>,
    /// Stable per-tab UUID — sourced from `TabState.id` on first
    /// load, generated fresh on tab creation. Exported into the
    /// shell as `_TAB_ID` so tools can call `POST /tabs/by-id/{id}/
    /// status` without caring about renames.
    id: String,
    /// Transient agent status published by a tool inside the tab
    /// (via the local API). Drives the tab-strip LED. Cleared by
    /// the staleness sweep after 5 minutes of no updates.
    agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable: last agent session UUID associated with this tab.
    /// Persisted to tabs.json so auto-resume can pick the same
    /// session back up after a restart.
    agent_session_id: Option<String>,
    /// Durable: which agent CLI owns the session ("catbus" or
    /// "claude" today). Free-form string so future agents can
    /// register without a code change. Used by the resume path
    /// to decide which command to type.
    agent_kind: Option<String>,
    /// Durable: whether the agent was in plan / read-only mode
    /// at last save. Restored along with the session so the tab
    /// comes back in the same mode.
    agent_plan_mode: Option<bool>,
    /// Per-tab share secrets. Minted lazily by the right-click
    /// share-link menu and persisted to tabs.json so URLs survive
    /// restarts. Empty until first share.
    share_token_rw: String,
    share_token_ro: String,
    /// Manual lock — user-toggled via right-click / `POST /lock`.
    ///
    /// **Gate authors:** call `tab.effective_locked()` (via
    /// [`crate::schedule::LockState`]) instead of reading this raw
    /// field. The effective state factors in the off-hours
    /// [`Self::schedule`] auto-lock so a new gate can't accidentally
    /// honour only the manual flag.
    locked: bool,
    /// Off-hours auto-lock (Settings → Schedule). When the rule's
    /// current state is closed,
    /// [`crate::schedule::LockState::effective_locked`] reports
    /// `true` even if [`Self::locked`] is false. None ⇒ no schedule,
    /// tab is always-open from the schedule's perspective.
    schedule: Option<crate::schedule::TabSchedule>,
    /// Last value pushed to `view.set_locked()` — the per-tick
    /// mirror in `persist()` compares against this so an idle tab's
    /// effective-lock recompute is a no-op (skip `cx.notify`).
    last_pushed_locked: Option<bool>,
    /// Per-tab background color override (`#RRGGBB`). `None` ⇒ use
    /// the global `Preferences::tab_bg_color`, which itself falls
    /// back to Tomorrow Night Blue.
    bg_color: Option<String>,
    /// Free-text context the in-tab agent set via `set-context` (e.g.
    /// the PR/task it's on). Shown as a hover tooltip on the tab name.
    /// In-memory; set via the API + drained from the snapshot.
    context: Option<String>,
    /// One-shot resume command queued on tab restore — when the
    /// shell is up the next tick types `<command>\n` into the
    /// PTY, then clears this. Set in `insert_tab` from the
    /// restored `agent_kind` / `agent_session_id` pair.
    pending_agent_resume: Option<String>,
    /// Memoised grid-derived snapshot fields, keyed by the PTY ring's
    /// `total_len`. `persist()` rebuilt the API snapshot for every tab
    /// every 2 s, and the grid scans (`ansi_text_with_cursor(200)` +
    /// 2000-row `raw_screen_text`) dominate that cost. Since all grid
    /// changes arrive as PTY bytes through the ring, an unchanged
    /// `total_len` means the previous scan is still valid. `None` until
    /// the first scan.
    snap_cache: Option<crate::term_export::GridSnapshotCache>,
    /// Per-tab resource-limit overrides. The GUI doesn't *apply* these
    /// (cgroup limits are headless-only), but it round-trips them
    /// through tabs.json so a desktop session doesn't wipe limits a
    /// headless run set on the same machine.
    limits: crate::TabResourceLimits,
}

impl crate::schedule::LockState for Tab {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&crate::schedule::TabSchedule> {
        self.schedule.as_ref()
    }
}

impl Tab {
    /// Active time this tab has been used (live run + persisted prior runs).
    /// Counts only periods when the user typed in the last 30s — the same
    /// idle threshold `persist()` uses to flip activate/deactivate. Idle
    /// minutes (and time while the drop-down is hidden) don't accumulate,
    /// so a tab left open overnight shows ~the same number in the morning.
    fn uptime(&self) -> std::time::Duration {
        let live = self.last_activated.map(|t| t.elapsed()).unwrap_or_default();
        self.prior_uptime + self.active_duration + live
    }

    fn activate(&mut self) {
        if self.last_activated.is_none() {
            self.last_activated = Some(std::time::Instant::now());
        }
    }

    /// If this tab had its scrollback restore deferred until first focus,
    /// feed it through vte now. Cheaper than blocking the cold launch on
    /// every tab's parser pass.
    fn flush_pending_restore(&mut self, cx: &mut gpui::App) {
        if let Some(out) = self.pending_restore.take() {
            self.view.read(cx).restore_output(&out);
            // restore_output feeds the parser directly (not through the
            // PTY ring), so the ring's total_len doesn't move — drop the
            // snapshot cache so the next persist re-scans the restored grid.
            self.snap_cache = None;
        }
    }

    /// Type the queued auto-resume command into the shell, if any.
    /// Fires Ctrl-U first to clear whatever the user may have started
    /// typing, then the command + LF. Same pattern as the "Switch to
    /// catbus" menu item.
    fn flush_pending_agent_resume(&mut self, cx: &mut gpui::App) {
        if let Some(cmd) = self.pending_agent_resume.take() {
            let view = self.view.read(cx);
            view.send_input_bytes(vec![0x15]); // Ctrl-U
            let mut bytes = cmd.into_bytes();
            bytes.push(b'\n');
            view.send_input_bytes(bytes);
        }
    }

    fn deactivate(&mut self) {
        if let Some(t) = self.last_activated.take() {
            self.active_duration += t.elapsed();
        }
    }
}

enum MenuKind {
    Tab(usize),
    Background,
}

struct ContextMenu {
    kind: MenuKind,
    position: Point<Pixels>,
    open_upward: bool,
}

struct Toast {
    message: String,
    time: std::time::Instant,
    path: Option<PathBuf>,
}

#[derive(Clone)]
struct DraggedTab {
    idx: usize,
    name: String,
    theme: ThemeName,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::theme(self.theme);
        div()
            .px(px(12.0))
            .py(px(4.0))
            .bg(th.elevated_hsla())
            .text_color(th.fg_hsla())
            .text_size(px(13.0))
            .rounded(px(4.0))
            .opacity(0.8)
            .child(self.name.clone())
    }
}

/// Hover tooltip showing a tab's agent-set context (the PR / task the
/// in-tab agent declared via `tab-atelier set-context "…"`).
struct TabContextTooltip {
    text: String,
    theme: ThemeName,
}

impl Render for TabContextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::theme(self.theme);
        div()
            .max_w(px(440.0))
            .px(px(10.0))
            .py(px(6.0))
            .bg(th.elevated_hsla())
            .text_color(th.fg_hsla())
            .text_size(px(12.0))
            .border_1()
            .border_color(th.border_hsla())
            .rounded(px(4.0))
            .child(self.text.clone())
    }
}

struct ExitConfirm {
    tab_idx: usize,
}

struct AppState {
    tabs: Vec<Tab>,
    active: usize,
    context_menu: Option<ContextMenu>,
    renaming: Option<(usize, String)>,
    rename_select_all: bool,
    rename_focus: FocusHandle,
    visible: bool,
    windowed: bool,
    exit_confirm: Option<ExitConfirm>,
    close_confirm: Option<usize>,
    show_qr: bool,
    font_config: FontConfig,
    tracker: Option<WakatimeTracker>,
    api_token: String,
    /// `addr:port` bind strings for the three listeners. Sourced from
    /// saved preferences at startup; live changes require a restart
    /// since the `TcpListener`s are bound in spawned threads.
    api_addr: String,
    api_tls_addr: String,
    /// Public base URL for share links (e.g.
    /// `https://example.com/~user/tab-atelier`). Read at "Copy share
    /// link" menu time. Empty → use the LAN URL.
    share_url_base: String,
    /// Global default viewer background color (`#RRGGBB`). `None` →
    /// fall back to the Tomorrow Night Blue default. Per-tab
    /// `Tab::bg_color` wins when set.
    tab_bg_global: Option<String>,
    api_state: Arc<Mutex<api::TabSnapshot>>,
    #[cfg(feature = "energy")]
    power_pids: Arc<Mutex<Vec<u32>>>,
    #[cfg(feature = "energy")]
    power_watts: Arc<Mutex<Vec<power::TabPower>>>,
    #[cfg(feature = "energy")]
    battery_percent: Arc<Mutex<Option<u8>>>,
    blink_on: bool,
    toasts: Vec<Toast>,
    lang: Lang,
    theme_name: ThemeName,
    opacity: u8,
    hotkeys: Vec<u8>,
    show_preferences: bool,
    show_hotkey_picker: bool,
    hotkey_picker_focus: FocusHandle,
    hotkey_picker_error: Option<String>,
    browser: Rc<RefCell<Option<String>>>,
    code_editor: Rc<RefCell<Option<String>>>,
    pref_browser_text: String,
    pref_browser_focus: FocusHandle,
    pref_editor_text: String,
    pref_editor_focus: FocusHandle,
    /// Editable copies of the bind strings shown in the preferences
    /// dialog. Persisted only on Save and applied on next launch (the
    /// API listener threads bind once at startup).
    pref_api_addr_text: String,
    pref_api_addr_focus: FocusHandle,
    pref_api_tls_addr_text: String,
    pref_api_tls_addr_focus: FocusHandle,
    pref_share_url_base_text: String,
    pref_share_url_base_focus: FocusHandle,
    /// Saved remote `tab-atelier-headless` endpoints. Loaded from
    /// `preferences.json` at startup, edited via the "Remote endpoints"
    /// section of the Preferences modal, and persisted back on Save.
    remote_endpoints: Vec<crate::RemoteEndpoint>,
    hotkey_handle: Option<platform::HotkeyHandle>,
    /// When the per-tab uptime files were last written. Persisting uptime
    /// every 2s would burn through disk writes for a value that only
    /// advances by ~2s anyway; we batch writes to once every 30s.
    last_uptime_save: std::cell::Cell<Option<std::time::Instant>>,
    /// CRC32 of the last serialized `tabs.json` content. Skips the write+
    /// rotate when nothing in the tab list changed since last tick.
    last_state_hash: std::cell::Cell<u32>,
    /// Per-tab active connection count (metering), keyed by tab id. Refreshed
    /// on a timer from `/proc` (the desktop is unprivileged → connections
    /// only, no nft byte counts). Side map so the `Tab` struct is untouched.
    tab_connections: std::cell::RefCell<std::collections::HashMap<String, usize>>,
    /// Last time `tab_connections` was refreshed (throttled — the /proc scan
    /// is too heavy for every persist tick).
    last_conn_meter: std::cell::Cell<Option<std::time::Instant>>,
}

impl AppState {
    fn t(&self) -> &'static Strings {
        locale::strings(self.lang)
    }

    fn th(&self) -> &'static theme::Theme {
        theme::theme(self.theme_name)
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename_focus = cx.focus_handle();
        let hotkey_picker_focus = cx.focus_handle();
        let pref_browser_focus = cx.focus_handle();
        let pref_editor_focus = cx.focus_handle();
        let pref_api_addr_focus = cx.focus_handle();
        let pref_api_tls_addr_focus = cx.focus_handle();
        let pref_share_url_base_focus = cx.focus_handle();
        let prefs = load_preferences(&platform::config_dir());
        // Font: preferences.json `font_family`/`font_size` → zed
        // settings → fontconfig-resolved monospace (the generic
        // "monospace" can render with a too-wide cell advance).
        let font_config = resolve_font_config(&platform::config_dir(), &prefs);
        // Latch the cleared-env opt-in (+ user vars) before any tab
        // spawns below, so every PTY this process creates honours it.
        if prefs.clear_env.unwrap_or(false) {
            crate::CLEAR_ENV.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::set_clear_env_user_vars(prefs.clear_env_vars.clone());
        }
        let browser: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.browser.clone()));
        let code_editor: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.code_editor.clone()));
        let lang = match prefs.lang.as_deref() {
            Some("fr") => Lang::Fr,
            Some("en") => Lang::En,
            _ => locale::detect_lang(),
        };
        let theme_name = prefs.theme.as_deref().and_then(ThemeName::from_id).unwrap_or_default();
        let opacity = prefs.opacity.unwrap_or(0xb8);
        let hotkeys = if prefs.hotkeys.is_empty() {
            DEFAULT_HOTKEYS.to_vec()
        } else {
            prefs.hotkeys
        };

        // Resolved early so we can export _TAB_ID / TAB_ATELIER_API_URL /
        // TAB_ATELIER_API_TOKEN into each PTY at spawn time. The token
        // file is whatever load_or_generate_token() reads/writes; the
        // API server itself starts later in this same function with the
        // same values.
        let api_token = api::load_or_generate_token();
        let api_addr_resolved = prefs.api_addr.clone().unwrap_or_else(|| crate::DEFAULT_API_ADDR.into());
        let api_url_for_pty = api_url_for_local_clients(&api_addr_resolved);

        let (tabs, active, restored_windowed) =
            if let Some(saved) = load_state_with_outputs(&platform::config_base_dir(), &platform::state_base_dir()) {
                info!("restoring {} tab(s) from saved state", saved.tabs.len());
                let mut tabs = Vec::new();
                for ts in &saved.tabs {
                    let cwd = ts.cwd.as_ref().map(PathBuf::from);
                    let fc = font_config.clone();
                    let br = browser.clone();
                    let ce = code_editor.clone();
                    let colors = ts.colors_enabled;
                    let env = tab_env_extras(&ts.id, &api_url_for_pty, &api_token);
                    let view = cx.new(|cx| {
                        let mut tv =
                            TerminalView::new_with_colors_and_env(cwd.as_deref(), fc, br, ce, colors, env, window, cx);
                        tv.set_theme(theme_name);
                        tv
                    });
                    // Defer restore_output for non-active tabs — feeding the
                    // whole scrollback through vte for every tab synchronously
                    // is what makes cold launch slow when there's a lot of
                    // history. The active tab is restored eagerly so the user
                    // sees their last screen the moment the window paints.
                    let is_active = tabs.len() == saved.active;
                    let pending_restore = ts.output.clone().and_then(|output| {
                        if is_active {
                            debug!("restoring {} chars of output for '{}'", output.len(), ts.name);
                            view.read(cx).restore_output(&output);
                            None
                        } else {
                            Some(output)
                        }
                    });
                    // Push the persisted effective-lock state onto
                    // the view so input is blocked from the moment
                    // the tab loads, not just after the first
                    // persist tick. Routes through `LockState` so a
                    // tab restored OUTSIDE its schedule's open hours
                    // also boots locked, not just manually-locked
                    // tabs.
                    if crate::schedule::LockState::effective_locked(ts) {
                        view.read(cx).set_locked(true);
                    }
                    // Restore the no-internet sandbox: set the flag and
                    // respawn into bubblewrap so the tab comes back
                    // airgapped. Skipped (net left on) when bwrap isn't
                    // installed, so a persisted net-off tab doesn't boot
                    // into a dead shell on a host without bubblewrap.
                    if ts.net_disabled && crate::bwrap_available() {
                        view.update(cx, |v, cx| {
                            v.set_net_disabled(true);
                            v.respawn(cwd.as_deref(), cx);
                        });
                    }
                    // Auto-resume: if this tab had an agent session
                    // and kind persisted, queue the resume command
                    // to be typed into the freshly-spawned shell.
                    let pending_agent_resume = match (&ts.agent_kind, &ts.agent_session_id) {
                        (Some(kind), Some(sid)) => build_agent_resume_command(kind, sid, ts.agent_plan_mode),
                        _ => None,
                    };
                    tabs.push(Tab {
                        view,
                        id: ts.id.clone(),
                        name: ts.name.clone(),
                        created_at: std::time::Instant::now(),
                        prior_uptime: std::time::Duration::from_secs_f64(ts.uptime_secs.unwrap_or(0.0)),
                        active_duration: std::time::Duration::ZERO,
                        last_activated: None,
                        // Treat a restored tab as "just seen" at boot so
                        // it starts grey and only ages into the blue
                        // dormant state after DORMANT_AFTER_SECS without
                        // you opening it — otherwise every attached
                        // session would flash blue on every restart.
                        last_focused_at: Some(std::time::Instant::now()),
                        #[cfg(feature = "energy")]
                        energy_wh: ts.energy_wh.unwrap_or(0.0),
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: ts.energy_wh.unwrap_or(0.0),
                        // Seed with the hash of the just-restored output so the
                        // first persist tick after launch doesn't rewrite an
                        // identical file.
                        output_hash_last_saved: ts.output.as_deref().map_or(0, |s| crate::crc32(s.as_bytes())),
                        output_ring_len_last_saved: None,
                        pending_restore,
                        // Seed from saved state so an immediate persist tick
                        // before the new shell has a /proc/PID/cwd readable
                        // doesn't overwrite the restored value with None.
                        last_known_cwd_string: cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        last_known_cwd: cwd.clone(),
                        agent_state: None,
                        agent_session_id: ts.agent_session_id.clone(),
                        agent_kind: ts.agent_kind.clone(),
                        agent_plan_mode: ts.agent_plan_mode,
                        share_token_rw: ts.share_token_rw.clone(),
                        share_token_ro: ts.share_token_ro.clone(),
                        locked: ts.locked,
                        schedule: ts.schedule.clone(),
                        bg_color: ts.bg_color.clone(),
                        context: None,
                        last_pushed_locked: None,
                        pending_agent_resume,
                        snap_cache: None,
                        limits: ts.limits.clone(),
                    });
                }
                if tabs.is_empty() {
                    let fc = font_config.clone();
                    let br = browser.clone();
                    let ce = code_editor.clone();
                    let new_id = crate::default_tab_id();
                    let env = tab_env_extras(&new_id, &api_url_for_pty, &api_token);
                    let view = cx.new(|cx| {
                        let mut tv = TerminalView::new_with_colors_and_env(None, fc, br, ce, true, env, window, cx);
                        tv.set_theme(theme_name);
                        tv
                    });
                    tabs.push(Tab {
                        view,
                        name: locale::strings(lang).terminal.into(),
                        created_at: std::time::Instant::now(),
                        prior_uptime: std::time::Duration::ZERO,
                        active_duration: std::time::Duration::ZERO,
                        last_activated: None,
                        // Treat a restored tab as "just seen" at boot so
                        // it starts grey and only ages into the blue
                        // dormant state after DORMANT_AFTER_SECS without
                        // you opening it — otherwise every attached
                        // session would flash blue on every restart.
                        last_focused_at: Some(std::time::Instant::now()),
                        #[cfg(feature = "energy")]
                        energy_wh: 0.0,
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: 0.0,
                        output_hash_last_saved: 0,
                        output_ring_len_last_saved: None,
                        pending_restore: None,
                        last_known_cwd: None,
                        last_known_cwd_string: None,
                        id: new_id,
                        agent_state: None,
                        agent_session_id: None,
                        agent_kind: None,
                        agent_plan_mode: None,
                        share_token_rw: String::new(),
                        share_token_ro: String::new(),
                        locked: false,
                        schedule: None,
                        bg_color: None,
                        context: None,
                        last_pushed_locked: None,
                        pending_agent_resume: None,
                        snap_cache: None,
                        limits: crate::TabResourceLimits::default(),
                    });
                }
                let active = saved.active.min(tabs.len() - 1);
                tabs[active].activate();
                (tabs, active, saved.windowed)
            } else {
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let new_id = crate::default_tab_id();
                let env = tab_env_extras(&new_id, &api_url_for_pty, &api_token);
                let view = cx.new(|cx| {
                    let mut tv = TerminalView::new_with_colors_and_env(None, fc, br, ce, true, env, window, cx);
                    tv.set_theme(theme_name);
                    tv
                });
                (
                    vec![Tab {
                        view,
                        name: locale::strings(lang).terminal.into(),
                        created_at: std::time::Instant::now(),
                        prior_uptime: std::time::Duration::ZERO,
                        active_duration: std::time::Duration::ZERO,
                        last_activated: Some(std::time::Instant::now()),
                        last_focused_at: Some(std::time::Instant::now()),
                        #[cfg(feature = "energy")]
                        energy_wh: 0.0,
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: 0.0,
                        output_hash_last_saved: 0,
                        output_ring_len_last_saved: None,
                        pending_restore: None,
                        last_known_cwd: None,
                        last_known_cwd_string: None,
                        id: new_id,
                        agent_state: None,
                        agent_session_id: None,
                        agent_kind: None,
                        agent_plan_mode: None,
                        share_token_rw: String::new(),
                        share_token_ro: String::new(),
                        locked: false,
                        schedule: None,
                        bg_color: None,
                        context: None,
                        last_pushed_locked: None,
                        pending_agent_resume: None,
                        snap_cache: None,
                        limits: crate::TabResourceLimits::default(),
                    }],
                    0,
                    false,
                )
            };
        if restored_windowed {
            window.toggle_fullscreen();
        }

        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.persist(cx);
                }) else {
                    break;
                };
            }
        })
        .detach();

        // Fast input-drain — the persist tick above runs every 2 s
        // (disk writes, scrollback CRC, …) which means a keystroke
        // POSTed via /input OR pushed via the WS `in` frame can sit
        // in `pending_input` for up to two whole seconds before
        // hitting the PTY. That's the "typing is very slow" report.
        //
        // Separate 16 ms tick that does ONLY the input drain. Other
        // pending queues (lock toggles, schedule changes, status
        // updates, renames, closes) stay on the slow persist path
        // — they're not latency-critical. 16 ms (~one 60 Hz frame)
        // keeps the average keystroke→PTY delay to ~8 ms; the drain is
        // a lock + (usually empty) Vec check, so the higher cadence is
        // negligible. NOTE: this runs on the gpui main thread, so it
        // is still stalled whenever the 2 s persist blocks that thread
        // — the periodic ~500 ms latency spike under many active tabs.
        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.drain_inputs(cx);
                }) else {
                    break;
                };
            }
        })
        .detach();

        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let Ok(()) = this.update(cx, |app, cx| {
                    if app.exit_confirm.is_some() {
                        return;
                    }
                    for (i, tab) in app.tabs.iter().enumerate() {
                        if tab.view.read(cx).has_exited() {
                            app.exit_confirm = Some(ExitConfirm { tab_idx: i });
                            cx.notify();
                            break;
                        }
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        #[cfg(feature = "energy")]
        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.blink_on = !app.blink_on;
                    if app.battery_percent.lock().unwrap().is_some_and(|b| b < 10) {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        tabs[active].view.read(cx).focus_handle(cx).focus(window);

        // Pick up the api key from Zed's settings when present so the
        // user doesn't need a separate `~/.wakatime.cfg` entry. When
        // absent, wakatime-cli falls back to its own config. Tracking
        // ultimately needs both a key (anywhere) and the cli binary on
        // disk; WakatimeTracker::new returns None if the cli is missing.
        let key = load_wakatime_key(&platform::config_dir());
        let tracker = WakatimeTracker::new(key);
        if tracker.is_some() {
            info!("wakatime tracking enabled");
        }

        // api_token + api_addr were resolved earlier so they could be
        // exported into each PTY's env; reuse them here.
        let api_addr = api_addr_resolved;
        let api_tls_addr = prefs.api_tls_addr.unwrap_or_else(|| crate::DEFAULT_API_TLS_ADDR.into());
        // User-supplied TLS cert + key (Cloudflare Origin etc.). Both
        // paths must be present; a half-configured pair falls back to
        // self-signed with a warning so the operator notices.
        let api_tls_external = match (prefs.api_tls_cert_path.clone(), prefs.api_tls_key_path.clone()) {
            (Some(c), Some(k)) => Some((std::path::PathBuf::from(c), std::path::PathBuf::from(k))),
            (Some(_), None) | (None, Some(_)) => {
                log::warn!("API/TLS: api_tls_cert_path and api_tls_key_path must both be set; using self-signed");
                None
            }
            (None, None) => None,
        };
        let api_tls_client_ca: Option<std::path::PathBuf> =
            prefs.api_tls_client_ca_path.clone().map(std::path::PathBuf::from);
        let share_url_base = prefs.share_url_base.unwrap_or_default();
        let tab_bg_global = prefs.tab_bg_color;
        let remote_endpoints = prefs.remote_endpoints;
        info!("API server starting on {api_addr} (TLS {api_tls_addr})");
        let api_state = Arc::new(Mutex::new(api::TabSnapshot {
            tabs: Vec::<api::SnapshotTab>::new(),
            // Set by start_api_server before it serves; an empty master
            // is rejected by the auth gate's non-empty guard, so the brief
            // pre-start window can't authorise anyone.
            master_token: String::new(),
            active: 0,
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
        let api_read_only = crate::read_only();
        api::start_api_server(api_state.clone(), api_token.clone(), api_read_only, api_addr.clone());
        api::start_api_server_tls(
            api_state.clone(),
            api_token.clone(),
            api_read_only,
            api_tls_addr.clone(),
            api_tls_external,
            api_tls_client_ca,
        );

        #[cfg(feature = "energy")]
        let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        #[cfg(feature = "energy")]
        let power_watts: Arc<Mutex<Vec<power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
        #[cfg(feature = "energy")]
        let battery_percent: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));
        #[cfg(feature = "energy")]
        power::start_power_monitor(power_pids.clone(), power_watts.clone(), battery_percent.clone());

        Self {
            tabs,
            active,
            context_menu: None,
            renaming: None,
            rename_select_all: false,
            rename_focus,
            visible: true,
            windowed: restored_windowed,
            exit_confirm: None,
            close_confirm: None,
            show_qr: false,
            font_config,
            tracker,
            api_token,
            api_addr,
            api_tls_addr,
            share_url_base,
            tab_bg_global,
            api_state,
            #[cfg(feature = "energy")]
            power_pids,
            #[cfg(feature = "energy")]
            power_watts,
            #[cfg(feature = "energy")]
            battery_percent,
            blink_on: false,
            toasts: Vec::new(),
            lang,
            theme_name,
            opacity,
            hotkeys,
            show_preferences: false,
            show_hotkey_picker: false,
            hotkey_picker_focus,
            hotkey_picker_error: None,
            browser,
            code_editor,
            pref_browser_text: String::new(),
            pref_browser_focus,
            pref_editor_text: String::new(),
            pref_editor_focus,
            pref_api_addr_text: String::new(),
            pref_api_addr_focus,
            pref_api_tls_addr_text: String::new(),
            pref_api_tls_addr_focus,
            pref_share_url_base_text: String::new(),
            pref_share_url_base_focus,
            remote_endpoints,
            hotkey_handle: None,
            last_uptime_save: std::cell::Cell::new(None),
            last_state_hash: std::cell::Cell::new(0),
            tab_connections: std::cell::RefCell::new(std::collections::HashMap::new()),
            last_conn_meter: std::cell::Cell::new(None),
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.tabs.len(), None, window, cx);
    }

    /// Like `add_tab` but with an explicit cwd hint from the API
    /// (`POST /tabs` with `{cwd: ...}`). Falls back to the existing
    /// inherit-from-active behaviour when the path doesn't exist.
    fn add_tab_in(&mut self, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.tabs.len(), Some(cwd), window, cx);
    }

    fn add_tab_after_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.active + 1, None, window, cx);
    }

    fn insert_tab(&mut self, at: usize, hint: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = hint.filter(|p| p.is_dir()).or_else(|| {
            let pid = self.tabs[self.active].view.read(cx).pid();
            platform::process_cwd(pid).or_else(|| self.tabs[self.active].last_known_cwd.clone())
        });
        self.tabs[self.active].deactivate();
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let tn = self.theme_name;
        let new_id = crate::default_tab_id();
        let env = tab_env_extras(&new_id, &api_url_for_local_clients(&self.api_addr), &self.api_token);
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new_with_colors_and_env(cwd.as_deref(), fc, br, ce, true, env, window, cx);
            tv.set_theme(tn);
            tv
        });
        let idx = at.min(self.tabs.len());
        self.tabs.insert(
            idx,
            Tab {
                view,
                name: format!("{} {}", self.t().terminal_n, self.tabs.len()),
                created_at: std::time::Instant::now(),
                prior_uptime: std::time::Duration::ZERO,
                active_duration: std::time::Duration::ZERO,
                last_activated: Some(std::time::Instant::now()),
                last_focused_at: Some(std::time::Instant::now()),
                #[cfg(feature = "energy")]
                energy_wh: 0.0,
                #[cfg(feature = "energy")]
                energy_wh_last_saved: 0.0,
                output_hash_last_saved: 0,
                output_ring_len_last_saved: None,
                pending_restore: None,
                last_known_cwd_string: cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                last_known_cwd: cwd,
                id: new_id,
                agent_state: None,
                agent_session_id: None,
                agent_kind: None,
                agent_plan_mode: None,
                share_token_rw: String::new(),
                share_token_ro: String::new(),
                locked: false,
                schedule: None,
                bg_color: None,
                context: None,
                last_pushed_locked: None,
                pending_agent_resume: None,
                snap_cache: None,
                limits: crate::TabResourceLimits::default(),
            },
        );
        self.active = idx;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn move_tab(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        let new_to = if from < to { to - 1 } else { to };
        self.tabs.insert(new_to, tab);
        self.active = if self.active == from {
            new_to
        } else {
            let mut a = self.active;
            if from < a {
                a -= 1;
            }
            if new_to <= a {
                a += 1;
            }
            a
        };
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let was_active = self.active == idx;
        self.tabs[idx].deactivate();
        self.tabs[idx].view.read(cx).shutdown();
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > idx {
            self.active -= 1;
        }
        if was_active {
            self.tabs[self.active].activate();
            self.tabs[self.active].flush_pending_restore(cx);
        }
        self.context_menu = None;
        cx.notify();
    }

    /// Drain `pending_input` from the API snapshot and ship the bytes
    /// to each tab's PTY. Called on a fast 50 ms tick by the spawn in
    /// `init`, so WS / HTTP keystrokes don't wait up to 2 s for the
    /// next `persist` tick. The slow persist still drains every
    /// pending queue (a no-op for input once we've cleared it here).
    fn drain_inputs(&mut self, cx: &mut Context<Self>) {
        let inputs: Vec<(usize, Vec<u8>)> = {
            let mut snapshot = self.api_state.lock().unwrap();
            if snapshot.pending_input.is_empty() {
                return;
            }
            snapshot.pending_input.drain(..).collect()
        };
        for (idx, bytes) in inputs {
            if idx < self.tabs.len() {
                self.tabs[idx].view.read(cx).send_input_bytes(bytes);
            }
        }
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            let tab = &mut self.tabs[self.active];
            let idle = tab
                .view
                .read(cx)
                .last_input_time()
                .is_none_or(|t| t.elapsed().as_secs() >= 30);
            if idle && tab.last_activated.is_some() {
                tab.deactivate();
            } else if !idle && tab.last_activated.is_none() {
                tab.activate();
            }
        }
        #[cfg(feature = "energy")]
        {
            let watts = self.power_watts.lock().unwrap();
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if let Some(w) = watts.get(i).and_then(|p| p.watts) {
                    tab.energy_wh += w * 2.0 / 3600.0;
                }
            }
        }
        let state_base = platform::state_base_dir();
        // Refresh last_known_cwd for any tab whose PTY child is still alive,
        // so a later persist tick after the shell exits still has a value
        // to fall back on instead of blanking the cwd to None. Update the
        // stringified mirror only when the PathBuf actually changed, so
        // unchanged tabs allocate nothing here.
        for tab in &mut self.tabs {
            let pid = tab.view.read(cx).pid();
            if let Some(p) = platform::process_cwd(pid)
                && tab.last_known_cwd.as_deref() != Some(p.as_path())
            {
                tab.last_known_cwd_string = Some(p.to_string_lossy().into_owned());
                tab.last_known_cwd = Some(p);
            }
        }
        // Connection metering (throttled ~5 s — the /proc scan is too heavy
        // for every 2 s persist tick). Desktop is unprivileged, so it's
        // connections only (no nft byte counters).
        #[cfg(target_os = "linux")]
        if self.last_conn_meter.get().is_none_or(|t| t.elapsed().as_secs() >= 5) {
            self.last_conn_meter.set(Some(std::time::Instant::now()));
            let roots: Vec<(String, u32)> = self
                .tabs
                .iter()
                .map(|tab| (tab.id.clone(), tab.view.read(cx).pid()))
                .collect();
            *self.tab_connections.borrow_mut() = crate::net_meter::connection_counts(&roots);
        }
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let cwd = tab.last_known_cwd_string.clone();
                TabState {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    cwd,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                    net_disabled: tab.view.read(cx).net_disabled(),
                    agent_session_id: tab.agent_session_id.clone(),
                    agent_kind: tab.agent_kind.clone(),
                    agent_plan_mode: tab.agent_plan_mode,
                    share_token_rw: tab.share_token_rw.clone(),
                    share_token_ro: tab.share_token_ro.clone(),
                    locked: tab.locked,
                    schedule: tab.schedule.clone(),
                    bg_color: tab.bg_color.clone(),
                    limits: tab.limits.clone(),
                    ..TabState::default()
                }
            })
            .collect();
        let mut api_tabs: Vec<api::SnapshotTab> = Vec::with_capacity(self.tabs.len());
        for (tab, ts) in self.tabs.iter_mut().zip(tabs.iter()) {
            let view = tab.view.read(cx);
            let shell_pid = view.pid();
            let pty_ring = view.pty_ring();
            // Dirtiness key: bytes ever written through the PTY ring.
            // Unchanged ⇒ the grid is byte-identical, so skip the scans.
            let ring_len = pty_ring.lock().map_or(0, |r| r.total_len());
            // 200 lines for the joined `output` (logical lines — the
            // mobile remote word-wraps them, more is wasted bandwidth on
            // a phone screen). 2000 for `raw_output` so xterm.js's
            // scrollback has actual history to browse through.
            let fresh = if tab.snap_cache.as_ref().is_none_or(|c| c.ring_len != ring_len) {
                let (output, cursor) = view.ansi_text_with_cursor(Some(200));
                let (raw_output, raw_cursor) = view.raw_screen_text(Some(2000));
                let (cols, rows) = view.dims();
                Some(crate::term_export::GridSnapshotCache {
                    ring_len,
                    output,
                    cursor,
                    raw_output,
                    raw_cursor,
                    cols,
                    rows,
                })
            } else {
                None
            };
            // No further use of `view` past here, so the borrow of
            // `tab.view` ends and we can mutate `tab.snap_cache`.
            if let Some(c) = fresh {
                tab.snap_cache = Some(c);
            }
            let grid = tab.snap_cache.as_ref().expect("snap_cache populated above").clone();
            let bg_color = crate::effective_tab_bg(tab.bg_color.as_deref(), self.tab_bg_global.as_deref()).to_string();
            api_tabs.push(api::SnapshotTab {
                id: tab.id.clone(),
                name: ts.name.clone(),
                cwd: ts.cwd.clone(),
                // ANSI escapes are kept so the mobile remote can render
                // colours instead of the previous flat-grey text.
                output: grid.output,
                raw_output: grid.raw_output,
                raw_cursor: grid.raw_cursor,
                uptime_secs: tab.uptime().as_secs_f64(),
                cursor: grid.cursor,
                cols: grid.cols,
                rows: grid.rows,
                share_token_rw: ts.share_token_rw.clone(),
                share_token_ro: ts.share_token_ro.clone(),
                locked: ts.locked,
                schedule: ts.schedule.clone(),
                bg_color,
                context: tab.context.clone(),
                shell_pid,
                agent_state: tab.agent_state.clone(),
                agent_session_id: tab.agent_session_id.clone(),
                agent_kind: tab.agent_kind.clone(),
                viewers: pty_ring.lock().map_or(0, |r| r.viewer_count()),
                pty_ring: Some(pty_ring),
                net_disabled: ts.net_disabled,
                connections: self.tab_connections.borrow().get(&tab.id).copied().unwrap_or(0),
                // Desktop is unprivileged → no nft byte counters.
                tx_bytes: 0,
                tx_denied_bytes: 0,
                // Desktop allowlist isn't wired (headless-only feature).
                net_allow: crate::net_policy::AllowConfig::default(),
            });
        }

        let read_only = crate::read_only();
        let saved = SavedState {
            tabs,
            active: self.active,
            windowed: self.windowed,
        };
        // Skip the write+rotate when the serialized content is identical to
        // last tick — the common case once the user stops poking the UI.
        let serialized = serde_json::to_string_pretty(&saved).unwrap_or_default();
        let new_hash = crate::crc32(serialized.as_bytes());
        if !read_only && new_hash != self.last_state_hash.get() {
            save_state(&platform::config_base_dir(), &saved);
            self.last_state_hash.set(new_hash);
        }
        if !read_only {
            for tab in &mut self.tabs {
                // Cheap dirtiness gate BEFORE the expensive
                // copy_all_history(). The PTY ring's monotonic counter
                // only advances when new bytes reached the grid; if it
                // hasn't moved since the last save, the output is
                // byte-identical and there's nothing to save. This is
                // what stops idle tabs from each paying a full-grid
                // serialize every 2 s (the dominant cost at 49 tabs).
                // crc32 below stays authoritative — we never skip a
                // real change, we just avoid serializing unchanged
                // tabs.
                let ring_len = {
                    let view = tab.view.read(cx);
                    view.pty_ring().lock().map_or(0, |r| r.total_len())
                };
                if tab.output_ring_len_last_saved == Some(ring_len) {
                    continue;
                }
                let output = tab.view.read(cx).copy_all_history();
                if output.is_empty() {
                    // Don't record ring_len: an empty grid that later
                    // fills must still serialize on the next tick.
                    continue;
                }
                let h = crate::crc32(output.as_bytes());
                if h == tab.output_hash_last_saved {
                    // Content identical despite a ring advance (e.g.
                    // bytes that didn't alter the visible/scrollback
                    // text). Record the ring position so we don't
                    // re-serialize until genuinely new bytes arrive.
                    tab.output_ring_len_last_saved = Some(ring_len);
                    continue;
                }
                save_tab_output(&state_base, &tab.name, &output);
                tab.output_hash_last_saved = h;
                tab.output_ring_len_last_saved = Some(ring_len);
            }
        }
        // Uptime + energy are never written in read-only mode; in normal
        // mode each has its own throttle (30s for uptime, ≥0.1 Wh delta for
        // energy) plus an unconditional flush on shutdown.
        if !read_only {
            let should_save_uptime = self
                .last_uptime_save
                .get()
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(30));
            if should_save_uptime {
                for tab in &self.tabs {
                    save_tab_uptime(&state_base, &tab.name, tab.uptime().as_secs_f64());
                }
                self.last_uptime_save.set(Some(std::time::Instant::now()));
            }
            #[cfg(feature = "energy")]
            {
                const ENERGY_DELTA_WH: f64 = 0.1;
                for tab in &mut self.tabs {
                    if (tab.energy_wh - tab.energy_wh_last_saved).abs() >= ENERGY_DELTA_WH {
                        save_tab_energy(&state_base, &tab.name, tab.energy_wh);
                        tab.energy_wh_last_saved = tab.energy_wh;
                    }
                }
            }
            // Token usage: read the sidecar written by catbus-agent and
            // persist it to the standard per-tab state file so the rest of
            // the app (and the mobile remote) can read cumulative totals
            // without knowing about the ~/.claude/projects layout.
            #[cfg(feature = "catbus")]
            for tab in &self.tabs {
                let pid = tab.view.read(cx).pid();
                if let Some(session) = crate::catbus_agent::find_session(pid)
                    && let Some(usage) = crate::catbus_agent::read_session_tokens(&session)
                {
                    save_tab_tokens(&state_base, &tab.name, &usage);
                }
            }
        }

        // A SIGINT/SIGTERM came in; do the unconditional flush and quit.
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            log::info!("graceful shutdown requested by signal, flushing state");
            self.close_all_tabs(cx);
            return;
        }

        {
            let mut snapshot = self.api_state.lock().unwrap();
            snapshot.tabs = api_tabs;
            snapshot.active = self.active;
            // Invalidate the /tabs cache; next GET rebuilds it once.
            snapshot.cached_response = None;
            #[cfg(feature = "energy")]
            snapshot.power.clone_from(&self.power_watts.lock().unwrap());
            #[cfg(feature = "energy")]
            {
                snapshot.battery_percent = *self.battery_percent.lock().unwrap();
            }
        }

        #[cfg(feature = "energy")]
        {
            let pids: Vec<u32> = self.tabs.iter().map(|tab| tab.view.read(cx).pid()).collect();
            *self.power_pids.lock().unwrap() = pids;
        }

        {
            let mut snapshot = self.api_state.lock().unwrap();
            let mut closes: Vec<usize> = snapshot.pending_closes.drain(..).collect();
            let activate = snapshot.pending_activate.take();
            let inputs: Vec<(usize, Vec<u8>)> = snapshot.pending_input.drain(..).collect();
            let renames: Vec<(usize, String)> = snapshot.pending_renames.drain(..).collect();
            let status_updates: Vec<api::PendingStatusUpdate> = snapshot.pending_status_updates.drain(..).collect();
            let lock_changes: Vec<(String, bool)> = snapshot.pending_lock_changes.drain(..).collect();
            let net_changes: Vec<(String, bool)> = snapshot.pending_net_changes.drain(..).collect();
            let bg_color_changes: Vec<(String, Option<String>)> = snapshot.pending_bg_color_changes.drain(..).collect();
            let context_changes: Vec<(String, Option<String>)> = snapshot.pending_context_changes.drain(..).collect();
            let token_rotations: Vec<String> = snapshot.pending_token_rotations.drain(..).collect();
            let schedule_changes: Vec<(String, Option<crate::schedule::TabSchedule>)> =
                snapshot.pending_schedule_changes.drain(..).collect();
            drop(snapshot);
            // Apply lock toggles from the API/CLI onto the runtime
            // Tab's manual flag. The view's set_locked() push happens
            // in the per-tick mirror below — that's the single site
            // that funnels `effective_locked()` into the gpui view,
            // so a future caller can't accidentally toggle the view
            // without also covering schedule-driven locks.
            for (tab_id, locked) in lock_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.locked = locked;
                }
            }
            // Net on/off toggles from the API/CLI. Set the view's flag
            // and respawn the PTY so the bubblewrap netns jail takes
            // effect — the shell can't be re-jailed in place. No window
            // here (persist tick), so use the low-level respawn rather
            // than `respawn_tab_with_history`; refocus isn't needed for a
            // background toggle.
            for (tab_id, disabled) in net_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    let cwd = platform::process_cwd(tab.view.read(cx).pid()).or_else(|| std::env::current_dir().ok());
                    tab.view.update(cx, |v, cx| {
                        v.set_net_disabled(disabled);
                        v.respawn(cwd.as_deref(), cx);
                    });
                }
            }
            for (tab_id, color) in bg_color_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.bg_color = color;
                }
            }
            // Revoke per-tab share tokens on the runtime Tab so the
            // cleared state persists into tabs.json (the snapshot was
            // already cleared by the endpoint for instant 401s).
            for tab_id in token_rotations {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.share_token_rw.clear();
                    tab.share_token_ro.clear();
                }
            }
            for (tab_id, context) in context_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.context = context;
                }
            }
            // Schedule changes — None clears, Some sets. Mirrors the
            // `locked` / `bg_color` drain above: mutate the runtime
            // `Tab` so the next persist tick rebuilds `tabs.json` +
            // `api_tabs` with the new value.
            for (tab_id, sched) in schedule_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.schedule = sched;
                }
            }
            // Per-tick effective-lock mirror.
            //
            // The view's `set_locked()` gate is what stops LOCAL
            // typing in the desktop GUI. We want it driven by the
            // same `effective_locked()` that every API gate uses, so
            // off-hours schedule transitions pause local input the
            // same way a manual lock does — without a dedicated
            // schedule-only push path that future code might miss.
            //
            // Compares against `tab.last_pushed_locked` and skips
            // when unchanged so an idle tab's per-tick recompute is
            // a single bool compare (no gpui notify, no schedule
            // re-eval cost beyond the one in effective_locked()).
            for tab in &mut self.tabs {
                let want = crate::schedule::LockState::effective_locked(tab);
                if tab.last_pushed_locked != Some(want) {
                    tab.view.read(cx).set_locked(want);
                    tab.last_pushed_locked = Some(want);
                }
            }
            for upd in status_updates {
                let Some(tab) = self.tabs.iter_mut().find(|t| t.id == upd.tab_id) else {
                    continue;
                };
                // "__clear__" sentinel from a POST with state=idle.
                // Wipes BOTH the transient state and the durable
                // session attachment, so the LED actually disappears
                // on Claude Code's SessionEnd hook (otherwise the
                // grey "session attached" dot would stick around).
                if upd.label.as_deref() == Some("__clear__") {
                    tab.agent_state = None;
                    tab.agent_session_id = None;
                    tab.agent_kind = None;
                    tab.agent_plan_mode = None;
                } else {
                    tab.agent_state = Some(crate::AgentStateSnapshot {
                        state: upd.state,
                        label: upd.label,
                        updated_at: std::time::Instant::now(),
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
            // Working-subprocess sweep: if the agent CLI has a child
            // process alive (Bash tool running `cargo build`, a long
            // `pytest`, …) keep the LED on "thinking" by refreshing
            // the snapshot timestamp. Long-running tool calls would
            // otherwise fall through the 2-min staleness sweep below
            // because no hook fires between `PreToolUse` and
            // `PostToolUse`. Also covers manual subshell commands the
            // user starts inside an active agent tab.
            let now = std::time::Instant::now();
            // Keep the foreground tab "freshly seen" so it never reads as
            // dormant; every other tab's last_focused_at ages until you
            // switch to it. Drives the blue dormant LED below.
            if let Some(t) = self.tabs.get_mut(self.active) {
                t.last_focused_at = Some(now);
            }
            #[cfg(feature = "catbus")]
            for tab in &mut self.tabs {
                if tab.agent_kind.is_none() {
                    continue;
                }
                let pid = tab.view.read(cx).pid();
                if !crate::catbus_agent::agent_has_active_subprocess(pid) {
                    continue;
                }
                tab.agent_state = Some(match tab.agent_state.take() {
                    Some(mut snap) if snap.state != crate::AgentState::Error => {
                        snap.state = crate::AgentState::Thinking;
                        snap.updated_at = now;
                        snap
                    }
                    Some(snap) => snap, // keep Error sticky
                    None => crate::AgentStateSnapshot {
                        state: crate::AgentState::Thinking,
                        label: Some("subproc".into()),
                        updated_at: now,
                    },
                });
            }
            // Staleness sweep: drop transient LED state when the last
            // update is older than 2 min. Real Claude turns are
            // tool-heavy and the `PreToolUse` hook refreshes the LED
            // on every tool call, so 2 min of total silence is a
            // strong signal the agent is actually idle (or wedged) —
            // we want the LED to demote back to the grey "session
            // attached" dot quickly so the user notices.
            for tab in &mut self.tabs {
                if let Some(snap) = &tab.agent_state
                    && now.duration_since(snap.updated_at).as_secs() > 120
                {
                    tab.agent_state = None;
                }
            }
            // Process-presence sweep: clear the whole agent attachment
            // (state + session + kind) when the agent CLI is no longer
            // a descendant of the tab's shell. Catches Ctrl-D / crash
            // / "closed claude without /exit" cases where the SessionEnd
            // hook never gets a chance to run — without this the LED
            // would keep amber-blinking from a stale Stop event until
            // the 2-min staleness sweep above eventually fires.
            #[cfg(feature = "catbus")]
            for tab in &mut self.tabs {
                if tab.agent_kind.is_some() {
                    let pid = tab.view.read(cx).pid();
                    if !crate::catbus_agent::has_agent_descendant(pid) {
                        tab.agent_state = None;
                        tab.agent_session_id = None;
                        tab.agent_kind = None;
                        tab.agent_plan_mode = None;
                    }
                }
            }
            // Auto-resume sweep: type the queued resume command into
            // any tab whose shell has had ~500ms to print its prompt.
            // `flush_pending_agent_resume` takes the queued command,
            // so each tab fires at most once.
            for tab in &mut self.tabs {
                if tab.pending_agent_resume.is_some() && tab.created_at.elapsed().as_millis() >= 500 {
                    tab.flush_pending_agent_resume(cx);
                }
            }
            for (idx, name) in renames {
                self.rename_tab(idx, name);
            }
            closes.sort_unstable();
            closes.dedup();
            for idx in closes.into_iter().rev() {
                if idx < self.tabs.len() && self.tabs.len() > 1 {
                    self.close_tab(idx, cx);
                }
            }
            if let Some(idx) = activate
                && idx < self.tabs.len()
                && self.active != idx
            {
                self.tabs[self.active].deactivate();
                self.active = idx;
                self.tabs[idx].activate();
                self.tabs[idx].flush_pending_restore(cx);
                cx.notify();
            }
            for (idx, bytes) in inputs {
                if idx < self.tabs.len() {
                    self.tabs[idx].view.read(cx).send_input_bytes(bytes);
                }
            }
        }

        if let Some(ref tracker) = self.tracker {
            // Only ping Wakatime when the user has actually touched the
            // active tab in the last 30s. Otherwise the persist tick
            // would flood the API with heartbeats while the terminal
            // sits idle in the system tray.
            let view = self.tabs[self.active].view.read(cx);
            let recently_active = view.last_input_time().is_some_and(|t| t.elapsed().as_secs() < 30);
            if recently_active {
                let cwd = platform::process_cwd(view.pid());
                tracker.record_activity(cwd);
            }
        }
    }

    fn respawn_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = platform::process_cwd(old_pid).or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.read(cx).shutdown();
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let tn = self.theme_name;
        let env = tab_env_extras(
            &self.tabs[idx].id,
            &api_url_for_local_clients(&self.api_addr),
            &self.api_token,
        );
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new_with_colors_and_env(cwd.as_deref(), fc, br, ce, true, env, window, cx);
            tv.set_theme(tn);
            tv
        });
        self.tabs[idx].view = view;
        self.tabs[idx].created_at = std::time::Instant::now();
        self.tabs[idx].prior_uptime = std::time::Duration::ZERO;
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        #[cfg(feature = "energy")]
        {
            self.tabs[idx].energy_wh = 0.0;
        }
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn respawn_tab_with_history(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = platform::process_cwd(old_pid).or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.update(cx, |view, cx| {
            view.respawn(cwd.as_deref(), cx);
        });
        self.tabs[idx].created_at = std::time::Instant::now();
        self.tabs[idx].prior_uptime = std::time::Duration::ZERO;
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        #[cfg(feature = "energy")]
        {
            self.tabs[idx].energy_wh = 0.0;
        }
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Rename a tab in place, moving the per-tab output/uptime/power files
    /// across so history sticks to the tab through the rename. No-op for
    /// out-of-range index or when the name doesn't change.
    fn rename_tab(&mut self, idx: usize, new_name: String) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_name = self.tabs[idx].name.clone();
        if old_name == new_name {
            return;
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
        self.tabs[idx].name = new_name;
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let state_base = platform::state_base_dir();
        // Snapshot cwd from /proc one last time before child processes
        // disappear; fall back to the cached last_known_cwd otherwise.
        for tab in &mut self.tabs {
            let pid = tab.view.read(cx).pid();
            if let Some(p) = platform::process_cwd(pid)
                && tab.last_known_cwd.as_deref() != Some(p.as_path())
            {
                tab.last_known_cwd_string = Some(p.to_string_lossy().into_owned());
                tab.last_known_cwd = Some(p);
            }
        }
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let cwd = tab.last_known_cwd_string.clone();
                TabState {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    cwd,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                    net_disabled: tab.view.read(cx).net_disabled(),
                    agent_session_id: tab.agent_session_id.clone(),
                    agent_kind: tab.agent_kind.clone(),
                    agent_plan_mode: tab.agent_plan_mode,
                    share_token_rw: tab.share_token_rw.clone(),
                    share_token_ro: tab.share_token_ro.clone(),
                    locked: tab.locked,
                    bg_color: tab.bg_color.clone(),
                    limits: tab.limits.clone(),
                    ..TabState::default()
                }
            })
            .collect();
        if !crate::read_only() {
            save_state(
                &platform::config_base_dir(),
                &SavedState {
                    tabs,
                    active: self.active,
                    windowed: self.windowed,
                },
            );
            for tab in &self.tabs {
                let output = tab.view.read(cx).copy_all_history();
                if !output.is_empty() {
                    save_tab_output(&state_base, &tab.name, &output);
                }
            }
            // Always flush uptime + energy on shutdown — bypass throttles so
            // the last tick isn't lost.
            for tab in &self.tabs {
                save_tab_uptime(&state_base, &tab.name, tab.uptime().as_secs_f64());
            }
            #[cfg(feature = "energy")]
            for tab in &mut self.tabs {
                save_tab_energy(&state_base, &tab.name, tab.energy_wh);
                tab.energy_wh_last_saved = tab.energy_wh;
            }
        }

        if let Some(ref tracker) = self.tracker {
            tracker.shutdown();
        }
        for tab in &self.tabs {
            tab.view.read(cx).shutdown();
        }
        cx.quit();
    }

    fn do_screenshot(&mut self, full: bool, cx: &mut Context<Self>) {
        let tab_name = self.tabs[self.active].name.clone();
        let progress_time = std::time::Instant::now();
        self.toasts.push(Toast {
            message: self.t().taking_screenshot.into(),
            time: progress_time,
            path: None,
        });
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
            let _ = this.update(cx, |state, cx| {
                state.toasts.retain(|t| t.time != progress_time);
                cx.notify();
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    if full {
                        screenshot::take_screenshot_full(&tab_name)
                    } else {
                        screenshot::take_screenshot_tab(&tab_name, 32)
                    }
                })
                .await;
            let toast_time = std::time::Instant::now();
            let _ = this.update(cx, |state, cx| {
                let t = state.t();
                let (msg, path) = match result {
                    Ok(path) => (t.saved.to_string(), Some(path)),
                    Err(e) => (format!("{}: {e}", t.screenshot_failed), None),
                };
                state.toasts.push(Toast {
                    message: msg,
                    time: toast_time,
                    path,
                });
                cx.notify();
            });
            cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
            let _ = this.update(cx, |state, cx| {
                state.toasts.retain(|t| t.time != toast_time);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_tab_bar(&mut self, battery: Option<u8>, _window: &mut Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let battery_critical = battery.is_some_and(|b| b < 10);
        let blink_red = battery_critical && self.blink_on;

        let th = self.th();
        let tab_bg = th.surface_hsla();
        let tab_active_bg = th.elevated_hsla();
        let tab_blink_bg = th.danger_hsla();
        let tab_fg = th.fg_hsla();
        let tab_border = th.border_hsla();
        #[cfg(feature = "energy")]
        let watts_fg = th.fg_muted_hsla();

        #[cfg(feature = "energy")]
        let watts = self.power_watts.lock().unwrap().clone();

        let mut bar = div()
            .id("tab-bar")
            .flex()
            .flex_row()
            .flex_wrap()
            .w_full()
            // `min_h` instead of fixed `h` so the bar grows when tabs
            // wrap to a second/third row. One row is still 32 px, two
            // rows is 64 px, etc.
            .min_h(px(32.0))
            .bg(tab_bg)
            .border_t_1()
            .border_b_1()
            .border_color(tab_border)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                    if this
                        .context_menu
                        .as_ref()
                        .is_some_and(|m| matches!(m.kind, MenuKind::Tab(_)))
                    {
                        return;
                    }
                    this.context_menu = Some(ContextMenu {
                        kind: MenuKind::Background,
                        position: ev.position,
                        open_upward: true,
                    });
                    cx.notify();
                }),
            );

        let blink_on = self.blink_on;
        let theme_name = self.theme_name;
        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            // Visual lock marker — a 🔒 ahead of the name is enough to
            // make "this tab won't accept input" obvious at a glance,
            // and stays out of the rename text (still raw name in the
            // rename editor).
            let base_name = if let Some((ri, ref text)) = self.renaming {
                if ri == i { text.clone() } else { tab.name.clone() }
            } else {
                tab.name.clone()
            };
            let name = if tab.locked && self.renaming.as_ref().is_none_or(|(ri, _)| *ri != i) {
                format!("🔒 {base_name}")
            } else {
                base_name
            };
            // Agent-state LED to the left of the tab name. Visible
            // whenever a session is attached (agent_kind set) OR a
            // transient state is live; cleared only when the session
            // actually ends (the `idle` POST wipes agent_kind too).
            // Waiting alternates amber ↔ grey with the same 500 ms
            // `blink_on` toggle that drives the battery indicator;
            // thinking / error stay steady.
            let session_attached = tab.agent_kind.is_some();
            let agent_led = if tab.agent_state.is_some() || session_attached {
                let grey = Hsla::from(Rgba {
                    r: 0.45,
                    g: 0.45,
                    b: 0.45,
                    a: 1.0,
                });
                let color = match tab.agent_state.as_ref().map(|s| s.state) {
                    Some(crate::AgentState::Thinking) => Hsla::from(Rgba {
                        r: 0.306,
                        g: 0.788,
                        b: 0.690,
                        a: 1.0,
                    }),
                    Some(crate::AgentState::Waiting) => {
                        if blink_on {
                            Hsla::from(Rgba {
                                r: 0.851,
                                g: 0.467,
                                b: 0.024,
                                a: 1.0,
                            })
                        } else {
                            grey
                        }
                    }
                    Some(crate::AgentState::Error) => Hsla::from(Rgba {
                        r: 0.937,
                        g: 0.267,
                        b: 0.267,
                        a: 1.0,
                    }),
                    // Session attached but quiet (state swept to None
                    // after ≥2 min of agent silence). If you also haven't
                    // opened this tab in a while it's "dormant" — show
                    // blue so a long-untended session stands out among
                    // many tabs; the steady grey is kept for one you've
                    // looked at recently. Opening the tab refreshes
                    // last_focused_at (persist tick) → back to grey.
                    None => {
                        // A tab someone is watching over the web/remote
                        // viewer is being tended too — never dormant.
                        let watched = tab.view.read(cx).viewer_count() > 0;
                        let dormant = !watched
                            && tab
                                .last_focused_at
                                .is_none_or(|t| t.elapsed().as_secs() > DORMANT_AFTER_SECS);
                        if dormant {
                            Hsla::from(Rgba {
                                r: 0.36,
                                g: 0.60,
                                b: 1.0,
                                a: 1.0,
                            })
                        } else {
                            grey
                        }
                    }
                };
                Some(div().w(px(7.0)).h(px(7.0)).mr(px(5.0)).rounded_full().bg(color))
            } else {
                None
            };

            #[cfg(feature = "energy")]
            let power_label = watts.get(i).map(power::TabPower::label).unwrap_or_default();

            let drag_name = tab.name.clone();
            let tab_el = div()
                .id(ElementId::Name(format!("tab-{i}").into()))
                .flex()
                .items_center()
                .px(px(12.0))
                // Fixed height (not `h_full`) so the bar wraps into
                // 32 px rows instead of a single tall row, and a
                // min-width + flex-shrink:0 so flex-wrap actually
                // engages rather than compressing every tab.
                .h(px(32.0))
                .min_w(px(120.0))
                .flex_shrink_0()
                // Border on all four sides so every tab is fully framed:
                // left/right give the column separators, and top/bottom
                // give each (wrapped) row a horizontal rule — without the
                // bottom line, rows of tabs blurred together vertically.
                // The outer edges sit flush with the bar container's own
                // top/bottom border (same 1px, same colour → one line).
                .border_l_1()
                .border_t_1()
                .border_b_1()
                .bg(if blink_red {
                    tab_blink_bg
                } else if is_active {
                    tab_active_bg
                } else {
                    tab_bg
                })
                .border_r_1()
                .border_color(tab_border)
                .text_color(tab_fg)
                .text_size(px(13.0))
                .cursor_pointer()
                // Hover tooltip: the agent-set context (PR/task), if any.
                .when_some(tab.context.clone(), |el, ctx| {
                    el.tooltip(move |_window, cx| {
                        cx.new(|_| TabContextTooltip {
                            text: ctx.clone(),
                            theme: theme_name,
                        })
                        .into()
                    })
                })
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    if ev.click_count() >= 2 {
                        let name = this.tabs[i].name.clone();
                        this.renaming = Some((i, name));
                        this.rename_select_all = true;
                        this.rename_focus.focus(window);
                        cx.notify();
                    } else if this.active != i {
                        let window_handle = window.window_handle().downcast::<Self>();
                        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(200))
                                .await;
                            if let Some(wh) = window_handle {
                                let _ = cx.update(|cx| {
                                    let _ = wh.update(cx, |app, window, cx| {
                                        if app.renaming.is_some() || app.active == i {
                                            return;
                                        }
                                        app.tabs[app.active].deactivate();
                                        app.active = i;
                                        app.tabs[i].activate();
                                        app.tabs[i].flush_pending_restore(cx);
                                        app.context_menu = None;
                                        app.tabs[app.active].view.read(cx).focus_handle(cx).focus(window);
                                        cx.notify();
                                    });
                                });
                            }
                        })
                        .detach();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                        this.context_menu = Some(ContextMenu {
                            kind: MenuKind::Tab(i),
                            position: ev.position,
                            open_upward: true,
                        });
                        cx.notify();
                    }),
                )
                .on_drag(
                    DraggedTab {
                        idx: i,
                        name: drag_name,
                        theme: self.theme_name,
                    },
                    |tab, _offset, _window, cx| cx.new(|_| tab.clone()),
                )
                .drag_over::<DraggedTab>(move |style, dragged, _window, _cx| {
                    if dragged.idx == i {
                        return style;
                    }
                    let s = style.bg(theme::theme(dragged.theme).selection_hsla());
                    if i < dragged.idx {
                        s.border_l_2()
                    } else {
                        s.border_r_2()
                    }
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.move_tab(dragged.idx, i, window, cx);
                }))
                .when_some(agent_led, ParentElement::child)
                .child(name);

            #[cfg(feature = "energy")]
            let tab_el = tab_el.child(
                div()
                    .text_size(px(11.0))
                    .text_color(watts_fg)
                    .min_w(px(55.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(power_label),
            );

            bar = bar.child(tab_el);
        }

        let plus_btn = div()
            .id("tab-plus")
            .flex()
            .items_center()
            .justify_center()
            // Same fixed 32 px height as a tab. `h_full` made the
            // button stretch over the entire wrapped bar (so the "+"
            // ended up vertically centred in 64 px and looked too
            // low). Min-width + no-shrink keeps it discoverable when
            // the bar fills up.
            .h(px(32.0))
            .min_w(px(40.0))
            .flex_shrink_0()
            .border_l_1()
            .border_color(tab_border)
            .text_color(tab_fg)
            // Bumped from 18 → 22 px and weight 700; at 18 the glyph
            // was barely above the bar background and read as a
            // faint dash on most themes.
            .text_size(px(22.0))
            .font_weight(gpui::FontWeight::BOLD)
            .cursor_pointer()
            .hover(|s| s.bg(tab_active_bg))
            .on_click(cx.listener(|this, _ev: &ClickEvent, window, cx| {
                this.add_tab(window, cx);
            }))
            .child("+");

        bar.child(plus_btn)
    }

    fn render_context_menu(&self, window: &Window, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let menu = self.context_menu.as_ref()?;
        let th = self.th();
        let menu_bg = th.surface_hsla();
        let menu_fg = th.fg_hsla();
        let menu_hover = th.selection_hsla();
        let menu_border = th.border_hsla();

        let pos = menu.position;
        let menu_width = px(150.0);
        let menu_height_estimate = if matches!(menu.kind, MenuKind::Tab(_)) {
            px(400.0)
        } else {
            px(350.0)
        };
        let vp = window.viewport_size();
        let x = pos.x.min(vp.width - menu_width);
        let open_upward = menu.open_upward || pos.y + menu_height_estimate > vp.height;

        let mut container = div().id("context-menu").absolute().left(x);

        container = if open_upward {
            let bottom_offset = vp.height - pos.y;
            container.bottom(bottom_offset)
        } else {
            container.top(pos.y)
        };

        container = container
            .bg(menu_bg)
            .border_1()
            .border_color(menu_border)
            .rounded(px(4.0))
            .py(px(4.0))
            .min_w(px(150.0))
            .text_color(menu_fg)
            .text_size(px(13.0));

        let sep = || div().mx(px(8.0)).my(px(4.0)).h(px(1.0)).bg(menu_border);

        let mut has_tab_section = false;

        if let MenuKind::Tab(idx) = menu.kind {
            has_tab_section = true;
            container = container.child(
                div()
                    .id("menu-rename")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                            let name = this.tabs[idx].name.clone();
                            this.renaming = Some((idx, name));
                            this.rename_select_all = true;
                            this.context_menu = None;
                            this.rename_focus.focus(window);
                            cx.notify();
                        }),
                    )
                    .child(self.t().rename),
            );

            // Copy the tab's working directory to the clipboard.
            // Reads /proc/<pid>/cwd via the platform helper; falls
            // back to the last known cwd captured at spawn time when
            // the live read fails (process gone, /proc unreadable).
            container = container.child(
                div()
                    .id("menu-copy-path")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let pid = this.tabs[idx].view.read(cx).pid();
                            let path = platform::process_cwd(pid).or_else(|| this.tabs[idx].last_known_cwd.clone());
                            if let Some(p) = path {
                                cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_path),
            );

            // Copy a shareable LAN URL — points at the xterm.js viewer
            // (/tabs/by-id/<UUID>/view). UUID rather than tab index so
            // a leaked link is bound to one tab and can't address
            // another by tweaking 0/1/2. The link carries a per-tab
            // share token (not the master api.token): RW for the
            // interactive link, RO for the read-only one. The server
            // refuses RO on `/input` with 403, so the URL *and* the
            // permission level are bound — stripping `&ro=1` does not
            // grant write access. Tokens are minted lazily here on
            // first menu use and persisted via tabs.json so URLs
            // survive restarts.
            for (label, ro) in [(self.t().copy_share_link, false), (self.t().copy_share_link_ro, true)] {
                let lan_ip = api::local_ip();
                let port = port_of(&self.api_addr, crate::DEFAULT_API_PORT);
                let tab_id = self.tabs[idx].id.clone();
                let toast_msg = self.t().share_link_copied;
                let id = if ro { "menu-share-link-ro" } else { "menu-share-link" };
                // If the user configured a public base (reverse-proxy
                // URL) use that. Strip any trailing slash so we can
                // unconditionally prepend "/tabs/...".
                let share_base = self.share_url_base.trim_end_matches('/').to_string();
                container = container.child(
                    div()
                        .id(id)
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                // Mint the share token on the runtime Tab if
                                // not yet present so it survives the next
                                // persist tick (snapshot is rebuilt from the
                                // runtime Tab each tick — writing only into
                                // the snapshot would be overwritten in 2s).
                                // Mirror immediately into the snapshot so the
                                // first request against the freshly-copied
                                // URL doesn't 401 during that window.
                                let slot_ref = if ro {
                                    &mut this.tabs[idx].share_token_ro
                                } else {
                                    &mut this.tabs[idx].share_token_rw
                                };
                                if slot_ref.is_empty() {
                                    *slot_ref = crate::mint_share_token();
                                }
                                let token = slot_ref.clone();
                                {
                                    let mut snap = this.api_state.lock().unwrap();
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id) {
                                        if ro {
                                            t.share_token_ro.clone_from(&token);
                                        } else {
                                            t.share_token_rw.clone_from(&token);
                                        }
                                    }
                                }
                                let base = if share_base.is_empty() {
                                    format!("http://{lan_ip}:{port}")
                                } else {
                                    share_base.clone()
                                };
                                let url = if ro {
                                    format!("{base}/tabs/by-id/{tab_id}/view?token={token}&ro=1")
                                } else {
                                    format!("{base}/tabs/by-id/{tab_id}/view?token={token}")
                                };
                                cx.write_to_clipboard(ClipboardItem::new_string(url));
                                let toast_time = std::time::Instant::now();
                                this.toasts.push(Toast {
                                    message: toast_msg.into(),
                                    time: toast_time,
                                    path: None,
                                });
                                this.context_menu = None;
                                cx.notify();
                                // Auto-dismiss after 1s — copy confirmation is
                                // ephemeral; lingering reads as "something
                                // failed".
                                let weak = cx.entity().downgrade();
                                cx.spawn(async move |_, cx: &mut AsyncApp| {
                                    cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                                    let _ = weak.update(cx, |this, cx| {
                                        this.toasts.retain(|t| t.time != toast_time);
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }),
                        )
                        .child(label),
                );
            }

            if self.tabs.len() > 1 {
                container = container.child(
                    div()
                        .id("menu-close")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                this.close_confirm = Some(idx);
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .child(self.t().close),
                );
            }

            // Drop catbus-agent into this tab's shell. Ctrl-U clears any
            // half-typed input, then `catbus-agent\n` runs it. No exec —
            // the shell stays alive underneath, so exiting catbus returns
            // the user to their session.
            container = container.child(
                div()
                    .id("menu-catbus")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[idx]
                                .view
                                .read(cx)
                                .send_input_bytes(b"\x15catbus-agent\n".to_vec());
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{1f408}\u{fe0f}\u{1f68c}\u{fe0f} Catbus"),
            );

            // ⛑ Brain — same pattern as Catbus: Ctrl-U + the command +
            // newline, takes over the current tab. Inside the brain
            // tab the user sees the rescue log; the brain watches
            // every OTHER tab via the local HTTP API and POSTs
            // `continue` to any whose scrollback matches a known
            // agent-failure signature OR whose agent_state == "error".
            container = container.child(
                div()
                    .id("menu-brain")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[idx]
                                .view
                                .read(cx)
                                .send_input_bytes(b"\x15tab-atelier brain\n".to_vec());
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{26d1}\u{fe0f} Brain"),
            );

            let colors_enabled = self.tabs[idx].view.read(cx).colors_enabled();
            let toggle_label = if colors_enabled {
                self.t().disable_colors
            } else {
                self.t().enable_colors
            };
            container = container.child(
                div()
                    .id("menu-toggle-colors")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                            this.tabs[idx].view.read(cx).set_colors_enabled(!colors_enabled);
                            this.context_menu = None;
                            this.respawn_tab_with_history(idx, window, cx);
                        }),
                    )
                    .child(toggle_label),
            );

            // Lock toggle — flips Tab.locked and pushes the new value
            // into the view so every input path (keyboard, paste,
            // hotkeys, programmatic) refuses immediately. Mirrored
            // into the API snapshot so /input and the share-link
            // viewer both observe the new state without waiting for
            // the next persist tick.
            let locked = self.tabs[idx].locked;
            let lock_label = if locked { self.t().unlock_tab } else { self.t().lock_tab };
            let tab_id_for_lock = self.tabs[idx].id.clone();
            container = container.child(
                div()
                    .id("menu-toggle-lock")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let next = !locked;
                            this.tabs[idx].locked = next;
                            this.tabs[idx].view.read(cx).set_locked(next);
                            {
                                let mut snap = this.api_state.lock().unwrap();
                                if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id_for_lock) {
                                    t.locked = next;
                                }
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(lock_label),
            );

            // Internet on/off — flips the tab's bubblewrap net-namespace
            // jail. Set the flag on the view, mirror into the API snapshot
            // (so /tabs and the toggle endpoint agree immediately), then
            // respawn history-preserving so the new netns takes effect —
            // the running shell can't be re-jailed in place. Shown only
            // when bubblewrap is usable, or when the tab is already off
            // (so it can always be turned back on); on a host without
            // bubblewrap and a net-on tab there's nothing to toggle to.
            let net_disabled = self.tabs[idx].view.read(cx).net_disabled();
            if net_disabled || crate::bwrap_available() {
                let net_label = if net_disabled {
                    self.t().enable_internet
                } else {
                    self.t().disable_internet
                };
                let tab_id_for_net = self.tabs[idx].id.clone();
                container = container.child(
                    div()
                        .id("menu-toggle-net")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                let next = !net_disabled;
                                this.tabs[idx].view.read(cx).set_net_disabled(next);
                                {
                                    let mut snap = this.api_state.lock().unwrap();
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id_for_net) {
                                        t.net_disabled = next;
                                    }
                                }
                                this.context_menu = None;
                                this.respawn_tab_with_history(idx, window, cx);
                            }),
                        )
                        .child(net_label),
                );
            }

            // (Background-color + Schedule preset rows used to live
            // here but pushed the context menu taller than a small-
            // laptop viewport. Both settings have CLI entry points
            // that scale better:
            //   tab-atelier-headless bg-color <tab> #RRGGBB
            //   tab-atelier-headless schedule <tab> "Mo-Fr 9-18" --tz …
            // The global Theme picker stays in the Preferences modal.)
        }

        {
            let stats_idx = match menu.kind {
                MenuKind::Tab(idx) => idx,
                MenuKind::Background => self.active,
            };
            let stat_fg = th.fg_muted_hsla();
            let elapsed = self.tabs[stats_idx].uptime();
            let t = self.t();

            let mut stats_lines: Vec<String> = Vec::new();

            #[cfg(feature = "energy")]
            {
                let power_info = self.power_watts.lock().unwrap().get(stats_idx).cloned();
                if let Some(ref p) = power_info {
                    if p.cpu_percent >= 0.1 {
                        stats_lines.push(format!("{}: {}", t.cpu, p.cpu_label()));
                    }
                    let wl = p.watts_label();
                    if !wl.is_empty() {
                        stats_lines.push(format!("{}: {wl}", t.power));
                    }
                }
                let wh = self.tabs[stats_idx].energy_wh;
                if wh > 0.0 {
                    if wh >= 1.0 {
                        stats_lines.push(format!("{}: {wh:.1} Wh", t.energy));
                    } else {
                        stats_lines.push(format!("{}: {:.0} mWh", t.energy, wh * 1000.0));
                    }
                }
            }
            stats_lines.push(format!("{}: {}", t.uptime, format_duration(elapsed)));
            let conns = self
                .tab_connections
                .borrow()
                .get(&self.tabs[stats_idx].id)
                .copied()
                .unwrap_or(0);
            if conns > 0 {
                stats_lines.push(format!("{}: {conns}", t.connections));
            }

            if !stats_lines.is_empty() {
                if has_tab_section {
                    container = container.child(sep());
                }
                for (si, line) in stats_lines.iter().enumerate() {
                    container = container.child(
                        div()
                            .id(SharedString::from(format!("menu-stat-{si}")))
                            .px(px(12.0))
                            .py(px(2.0))
                            .text_size(px(11.0))
                            .text_color(stat_fg)
                            .child(line.clone()),
                    );
                }
            }
        }

        // Clipboard section
        container = container
            .child(sep())
            .child(
                div()
                    .id("menu-copy")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            if let Some(text) = this.tabs[this.active].view.read(cx).copy_selection() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy),
            )
            .child(
                div()
                    .id("menu-copy-all")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            // Clipboard gets plain text so other apps don't see
                            // raw `\x1b[...m` escapes. The persistence call
                            // sites that need colours go through copy_all_history
                            // directly.
                            let text = crate::strip_ansi(&this.tabs[this.active].view.read(cx).copy_all_history());
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_all),
            );

        // Paste row — XOR'd by whether the active tab has a live
        // selection.
        //
        // No selection ⇒ surface "Paste" (system clipboard). Useful
        // for piping commands back in from a separate editor.
        //
        // Selection present ⇒ surface "Paste selection" instead and
        // suppress plain "Paste". The user just highlighted something
        // they want to act on — offering both reads as a near-miss
        // (one wrong click and you've overwritten the clipboard with
        // an unrelated paste), so we collapse to the single
        // contextually-correct action.
        let has_active_selection = self.tabs[self.active].view.read(cx).copy_selection().is_some();
        if has_active_selection {
            container = container.child(
                div()
                    .id("menu-paste-selection")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            let view = &this.tabs[this.active].view;
                            if let Some(text) = view.read(cx).copy_selection() {
                                view.read(cx).send_clipboard(&text);
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().paste_selection),
            );
        } else {
            container = container.child(
                div()
                    .id("menu-paste")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            if let Some(item) = cx.read_from_clipboard()
                                && let Some(text) = TerminalView::clipboard_to_paste_text(&item)
                            {
                                let view = &this.tabs[this.active].view;
                                view.read(cx).send_clipboard(&text);
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().paste),
            );
        }
        container = container
            // Terminal section
            .child(sep())
            .child(
                div()
                    .id("menu-reset")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[this.active].view.read(cx).reset_terminal();
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().reset_input_color),
            )
            // Screenshot section
            .child(sep())
            .child(
                div()
                    .id("menu-screenshot-tab")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            this.do_screenshot(false, cx);
                        }),
                    )
                    .child(self.t().screenshot_tab),
            )
            .child(
                div()
                    .id("menu-screenshot-app")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            this.do_screenshot(true, cx);
                        }),
                    )
                    .child(self.t().screenshot_app),
            )
            // Window section
            .child(sep())
            .child(
                div()
                    .id("menu-windowed")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                            this.windowed = !this.windowed;
                            window.toggle_fullscreen();
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(if self.windowed {
                        self.t().fullscreen_mode
                    } else {
                        self.t().windowed_mode
                    }),
            )
            .child(
                div()
                    .id("menu-close-all")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.close_all_tabs(cx);
                        }),
                    )
                    .child(self.t().close_all),
            )
            // App section
            .child(sep())
            .child(
                div()
                    .id("menu-remote")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.show_qr = true;
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().remote_control),
            )
            .child(
                div()
                    .id("menu-preferences")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                            this.pref_browser_text = this.browser.borrow().clone().unwrap_or_default();
                            this.pref_editor_text = this.code_editor.borrow().clone().unwrap_or_default();
                            this.pref_api_addr_text = this.api_addr.clone();
                            this.pref_api_tls_addr_text = this.api_tls_addr.clone();
                            this.pref_share_url_base_text = this.share_url_base.clone();
                            this.show_preferences = true;
                            this.context_menu = None;
                            // Move focus into the first prefs input on
                            // open so the user can type immediately
                            // without clicking. Without this, the
                            // terminal still has focus and the inputs
                            // *appear* unfocusable because their
                            // on_mouse_down focus call fires AFTER
                            // gpui dispatches the first click's keys
                            // — by which point the keys are already
                            // queued at the terminal.
                            this.pref_api_addr_focus.focus(window);
                            cx.notify();
                        }),
                    )
                    .child(self.t().preferences),
            );

        Some(container)
    }

    fn render_rename_input(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let (_idx, text) = self.renaming.as_ref()?;
        let text = text.clone();
        let th = self.th();
        let input_bg = th.surface_hsla();
        let input_fg = th.fg_hsla();
        let input_border = th.accent_hsla();
        let cursor_color = th.fg_hsla();

        Some(
            div()
                .id("rename-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                }))
                .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, _cx| {})
                .child(
                    div()
                        .id("rename-box")
                        .key_context("rename")
                        .track_focus(&self.rename_focus)
                        .bg(input_bg)
                        .border_1()
                        .border_color(input_border)
                        .rounded(px(4.0))
                        .p(px(16.0))
                        .min_w(px(300.0))
                        .text_color(input_fg)
                        .text_size(px(14.0))
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            let key = ev.keystroke.key.as_str();
                            match key {
                                "enter" => {
                                    if let Some((i, ref text)) = this.renaming
                                        && i < this.tabs.len()
                                    {
                                        this.rename_tab(i, text.clone());
                                    }
                                    this.renaming = None;
                                    this.rename_select_all = false;
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "escape" => {
                                    this.renaming = None;
                                    this.rename_select_all = false;
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "backspace" => {
                                    if let Some((_, ref mut text)) = this.renaming {
                                        if this.rename_select_all {
                                            text.clear();
                                            this.rename_select_all = false;
                                        } else {
                                            text.pop();
                                        }
                                    }
                                    cx.notify();
                                }
                                _ => {
                                    if let Some(ref ch) = ev.keystroke.key_char {
                                        if let Some((_, ref mut text)) = this.renaming {
                                            if this.rename_select_all {
                                                text.clear();
                                                this.rename_select_all = false;
                                            }
                                            text.push_str(ch);
                                        }
                                        cx.notify();
                                    }
                                }
                            }
                        }))
                        .child(self.t().rename_tab)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .mt(px(8.0))
                                .bg(th.bg_hsla())
                                .border_1()
                                .border_color(input_border)
                                .rounded(px(3.0))
                                .px(px(8.0))
                                .py(px(4.0))
                                .min_h(px(28.0))
                                .cursor_text()
                                .when(self.rename_select_all, |el| {
                                    el.child(div().bg(th.selection_hsla()).px(px(2.0)).child(text.clone()))
                                })
                                .when(!self.rename_select_all, |el| {
                                    el.child(text).child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
                                }),
                        ),
                ),
        )
    }

    fn render_exit_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let confirm = self.exit_confirm.as_ref()?;
        let idx = confirm.tab_idx;
        if idx >= self.tabs.len() {
            return None;
        }
        let tab_name = self.tabs[idx].name.clone();

        let th = self.th();
        let dialog_bg = th.surface_hsla();
        let dialog_fg = th.fg_hsla();
        let dialog_border = th.border_hsla();
        let btn_bg = th.accent_hsla();
        let btn_hover = th.accent_hover_hsla();
        let btn_secondary_bg = th.border_hsla();
        let btn_secondary_hover = th.selection_hsla();

        Some(
            div()
                .id("exit-confirm-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                }))
                .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, _cx| {})
                .child(
                    div()
                        .id("exit-confirm-box")
                        .bg(dialog_bg)
                        .border_1()
                        .border_color(dialog_border)
                        .rounded(px(6.0))
                        .p(px(20.0))
                        .min_w(px(320.0))
                        .text_color(dialog_fg)
                        .text_size(px(14.0))
                        .child(
                            div()
                                .text_size(px(15.0))
                                .child(format!("Shell exited in \"{tab_name}\"")),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(13.0))
                                .text_color(th.fg_muted_hsla())
                                .child(self.t().exit_close_or_reopen),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(8.0))
                                .mt(px(16.0))
                                .justify_end()
                                .child(
                                    div()
                                        .id("exit-reopen-clean")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_secondary_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_secondary_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                                this.respawn_tab(idx, window, cx);
                                            }),
                                        )
                                        .child(self.t().reopen_clean),
                                )
                                .child(
                                    div()
                                        .id("exit-reopen-history")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_secondary_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_secondary_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                                this.respawn_tab_with_history(idx, window, cx);
                                            }),
                                        )
                                        .child(self.t().reopen_with_history),
                                )
                                .child(
                                    div()
                                        .id("exit-close")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                                this.exit_confirm = None;
                                                if this.tabs.len() <= 1 {
                                                    this.close_all_tabs(cx);
                                                } else {
                                                    this.close_tab(idx, cx);
                                                }
                                                if !this.tabs.is_empty() {
                                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                                }
                                            }),
                                        )
                                        .child(self.t().close_tab),
                                ),
                        ),
                ),
        )
    }

    fn render_close_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let idx = self.close_confirm?;
        if idx >= self.tabs.len() {
            return None;
        }
        let tab_name = self.tabs[idx].name.clone();

        let th = self.th();
        let dialog_bg = th.surface_hsla();
        let dialog_fg = th.fg_hsla();
        let dialog_border = th.border_hsla();
        let btn_bg = th.accent_hsla();
        let btn_hover = th.accent_hover_hsla();
        let btn_secondary_bg = th.border_hsla();
        let btn_secondary_hover = th.selection_hsla();

        Some(
            div()
                .id("close-confirm-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                }))
                .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, _cx| {})
                .child(
                    div()
                        .id("close-confirm-box")
                        .bg(dialog_bg)
                        .border_1()
                        .border_color(dialog_border)
                        .rounded(px(6.0))
                        .p(px(20.0))
                        .min_w(px(320.0))
                        .text_color(dialog_fg)
                        .text_size(px(14.0))
                        .child(div().text_size(px(15.0)).child(format!("Close \"{tab_name}\"?")))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(8.0))
                                .mt(px(16.0))
                                .justify_end()
                                .child(
                                    div()
                                        .id("close-cancel")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_secondary_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_secondary_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                                this.close_confirm = None;
                                                cx.notify();
                                            }),
                                        )
                                        .child(self.t().cancel),
                                )
                                .child(
                                    div()
                                        .id("close-confirm-btn")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                                this.close_confirm = None;
                                                this.close_tab(idx, cx);
                                            }),
                                        )
                                        .child(self.t().close),
                                ),
                        ),
                ),
        )
    }

    fn render_qr_modal(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if !self.show_qr {
            return None;
        }

        // Refresh on every render so a freshly-opened modal reflects the
        // current routing table (Wi-Fi switch, VPN up/down, …) rather
        // than whatever IPs were live when the process started.
        let ips = api::local_ips_all();
        let primary_ip = ips.first().cloned().unwrap_or_else(|| "127.0.0.1".into());
        let lan_url = format!(
            "http://{primary_ip}:{}",
            port_of(&self.api_addr, crate::DEFAULT_API_PORT)
        );
        let lan_url_tls = format!(
            "https://{primary_ip}:{}",
            port_of(&self.api_tls_addr, crate::DEFAULT_API_PORT + 1)
        );
        // Pass both the plain-HTTP and TLS URLs into the deep link; the
        // mobile client picks whichever its current build supports.
        let qr_payload = format!(
            "taremote://onboard?url={lan_url}&tls_url={lan_url_tls}&token={}",
            self.api_token
        );
        let url = format!("{lan_url}?token={}", self.api_token);
        let url_for_click = url.clone();

        let Ok(qr) = qrcode::QrCode::new(qr_payload.as_bytes()) else {
            return None;
        };

        let th = self.th();
        let dialog_bg = th.surface_hsla();
        let dialog_fg = th.fg_hsla();
        let dialog_border = th.border_hsla();
        let btn_bg = th.accent_hsla();
        let btn_hover = th.accent_hover_hsla();
        let link_fg = th.accent_hsla();

        let colors = qr.to_colors();
        let w = qr.width();
        let module_size = px(4.0);
        let mut qr_grid = div()
            .mt(px(12.0))
            .bg(gpui::white())
            .rounded(px(4.0))
            .p(px(16.0))
            .flex()
            .flex_col();
        for row in 0..w {
            let mut row_div = div().flex().flex_row();
            for col in 0..w {
                let is_dark = colors[row * w + col] == qrcode::Color::Dark;
                row_div = row_div.child(
                    div()
                        .w(module_size)
                        .h(module_size)
                        .when(is_dark, |el| el.bg(gpui::black())),
                );
            }
            qr_grid = qr_grid.child(row_div);
        }

        Some(
            div()
                .id("qr-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        this.show_qr = false;
                        cx.notify();
                    }),
                )
                .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, _cx| {})
                .child(
                    div()
                        .id("qr-box")
                        .bg(dialog_bg)
                        .border_1()
                        .border_color(dialog_border)
                        .rounded(px(6.0))
                        .p(px(20.0))
                        .text_color(dialog_fg)
                        .text_size(px(14.0))
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .child(div().text_size(px(15.0)).child(self.t().scan_to_connect))
                        .child(qr_grid)
                        .child(
                            div()
                                .id("qr-url")
                                .mt(px(8.0))
                                .text_size(px(11.0))
                                .text_color(link_fg)
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _ev: &MouseDownEvent, _window, _cx| {
                                        let browser = this.browser.borrow().clone();
                                        platform::open_url(&url_for_click, browser.as_deref());
                                    }),
                                )
                                .child(url),
                        )
                        .when(ips.len() > 1, |el| {
                            // Surface every interface IP so the user can
                            // see which network they're reachable on
                            // (Wi-Fi vs Ethernet vs Docker bridge, etc.).
                            let mut list = div()
                                .mt(px(8.0))
                                .text_size(px(10.0))
                                .text_color(dialog_fg)
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().text_color(dialog_fg).child("Also reachable at:"));
                            for ip in ips.iter().skip(1) {
                                list = list.child(div().text_color(link_fg).child(format!(
                                    "http://{ip}:{}",
                                    port_of(&self.api_addr, crate::DEFAULT_API_PORT)
                                )));
                            }
                            el.child(list)
                        })
                        .child(
                            div().flex().justify_end().mt(px(12.0)).child(
                                div()
                                    .id("qr-close")
                                    .px(px(14.0))
                                    .py(px(6.0))
                                    .bg(btn_bg)
                                    .rounded(px(3.0))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(btn_hover))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                            this.show_qr = false;
                                            cx.notify();
                                        }),
                                    )
                                    .child(self.t().close),
                            ),
                        ),
                ),
        )
    }

    fn render_preferences(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if !self.show_preferences {
            return None;
        }

        let overlay_bg = Hsla::from(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        });
        let th = self.th();
        let modal_bg = th.surface_hsla();
        let modal_fg = th.fg_hsla();
        let modal_border = th.border_hsla();
        let input_border = th.accent_hsla();
        let btn_bg = th.accent_hsla();
        let btn_hover = th.accent_hover_hsla();
        let option_bg = th.elevated_hsla();
        let option_active = th.accent_hsla();
        let placeholder_fg = th.fg_muted_hsla();
        let cursor_color = th.fg_hsla();
        let t = self.t();

        let mut theme_options = div().flex().flex_col().gap(px(4.0)).mt(px(8.0));

        for &tn in ThemeName::ALL {
            let is_active = tn == self.theme_name;
            theme_options = theme_options.child(
                div()
                    .id(SharedString::from(format!("pref-theme-{}", tn.id())))
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .bg(if is_active { option_active } else { option_bg })
                    .hover(|s| s.bg(if is_active { option_active } else { btn_hover }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.theme_name = tn;
                            for tab in &this.tabs {
                                tab.view.update(cx, |tv, _cx| tv.set_theme(tn));
                            }
                            cx.notify();
                        }),
                    )
                    .child(tn.label()),
            );
        }

        let opacity_pct = (self.opacity as f32 / 255.0 * 100.0).round() as u8;
        let mut opacity_slider = div().flex().flex_row().items_center().gap(px(8.0)).mt(px(8.0));
        let mut track = div().flex().flex_row().h(px(20.0)).rounded(px(3.0)).overflow_hidden();
        for i in 0..100u8 {
            let val = ((i as f32 + 1.0) / 100.0 * 255.0).round() as u8;
            let filled = val <= self.opacity;
            track = track.child(
                div()
                    .id(SharedString::from(format!("pref-opacity-{i}")))
                    .w(px(2.72))
                    .h_full()
                    .cursor_pointer()
                    .bg(if filled { option_active } else { option_bg })
                    .hover(|s| s.bg(btn_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.opacity = val;
                            cx.notify();
                        }),
                    ),
            );
        }
        opacity_slider = opacity_slider.child(track).child(format!("{opacity_pct}%"));

        let mut hotkey_list = div().flex().flex_col().gap(px(4.0)).mt(px(8.0));
        for &kc in &self.hotkeys {
            let label = keycode_label(kc);
            let can_remove = self.hotkeys.len() > 1;
            hotkey_list = hotkey_list.child(
                div()
                    .id(SharedString::from(format!("pref-hk-{kc}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(3.0))
                    .bg(option_bg)
                    .child(label)
                    .when(can_remove, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("pref-hk-rm-{kc}")))
                                .cursor_pointer()
                                .px(px(6.0))
                                .rounded(px(3.0))
                                .hover(|s| s.bg(btn_hover))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                        this.hotkeys.retain(|&k| k != kc);
                                        cx.notify();
                                    }),
                                )
                                .child("\u{00d7}"),
                        )
                    }),
            );
        }
        hotkey_list = hotkey_list.child(
            div()
                .id("pref-hk-add")
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(3.0))
                .cursor_pointer()
                .bg(btn_bg)
                .hover(|s| s.bg(btn_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                        this.show_hotkey_picker = true;
                        this.hotkey_picker_error = None;
                        if let Some(ref handle) = this.hotkey_handle {
                            handle.suspend();
                        }
                        this.hotkey_picker_focus.focus(window);
                        cx.notify();
                    }),
                )
                .child(format!("+ {}", t.add_key)),
        );

        let mut lang_options = div().flex().flex_col().gap(px(4.0)).mt(px(8.0));

        for &lang in Lang::ALL {
            let is_active = lang == self.lang;
            lang_options = lang_options.child(
                div()
                    .id(SharedString::from(format!("pref-lang-{}", lang.label())))
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .bg(if is_active { option_active } else { option_bg })
                    .hover(|s| s.bg(if is_active { option_active } else { btn_hover }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.lang = lang;
                            cx.notify();
                        }),
                    )
                    .child(lang.label()),
            );
        }

        let browser_text = self.pref_browser_text.clone();
        let browser_input = div()
            .id("pref-browser-input")
            .key_context("pref-browser")
            .track_focus(&self.pref_browser_focus)
            .mt(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .bg(th.bg_hsla())
            .border_1()
            .border_color(input_border)
            .rounded(px(3.0))
            .px(px(8.0))
            .py(px(4.0))
            .min_h(px(28.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.pref_browser_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_browser_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char {
                            this.pref_browser_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(browser_text.is_empty(), |el| {
                el.child(div().text_color(placeholder_fg).child(t.browser_placeholder))
            })
            .when(!browser_text.is_empty(), |el| {
                el.child(browser_text)
                    .child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
            });

        let api_addr_text = self.pref_api_addr_text.clone();
        let api_addr_input = div()
            .id("pref-api-addr-input")
            .key_context("pref-api-addr")
            .track_focus(&self.pref_api_addr_focus)
            .mt(px(8.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .bg(th.bg_hsla())
            .border_1()
            .border_color(input_border)
            .rounded(px(3.0))
            .px(px(8.0))
            .py(px(4.0))
            .min_h(px(28.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.pref_api_addr_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_api_addr_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char
                            && ch.chars().all(is_addr_port_char)
                            && this.pref_api_addr_text.len() + ch.len() <= MAX_ADDR_LEN
                        {
                            this.pref_api_addr_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(api_addr_text.is_empty(), |el| {
                el.child(div().text_color(placeholder_fg).child(crate::DEFAULT_API_ADDR))
            })
            .when(!api_addr_text.is_empty(), |el| {
                el.child(api_addr_text)
                    .child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
            });

        let api_tls_addr_text = self.pref_api_tls_addr_text.clone();
        let api_tls_addr_input = div()
            .id("pref-api-tls-addr-input")
            .key_context("pref-api-tls-addr")
            .track_focus(&self.pref_api_tls_addr_focus)
            .mt(px(8.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .bg(th.bg_hsla())
            .border_1()
            .border_color(input_border)
            .rounded(px(3.0))
            .px(px(8.0))
            .py(px(4.0))
            .min_h(px(28.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.pref_api_tls_addr_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_api_tls_addr_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char
                            && ch.chars().all(is_addr_port_char)
                            && this.pref_api_tls_addr_text.len() + ch.len() <= MAX_ADDR_LEN
                        {
                            this.pref_api_tls_addr_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(api_tls_addr_text.is_empty(), |el| {
                el.child(div().text_color(placeholder_fg).child(crate::DEFAULT_API_TLS_ADDR))
            })
            .when(!api_tls_addr_text.is_empty(), |el| {
                el.child(api_tls_addr_text)
                    .child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
            });

        // Free-form URL field — share-link base for reverse-proxied
        // setups (Caddy at https://example.com/~user/path). Permissive
        // char filter (letters, digits, URL-safe punctuation) and a
        // higher max length than the addr:port inputs.
        let share_url_base_text = self.pref_share_url_base_text.clone();
        let share_url_base_input = div()
            .id("pref-share-url-base-input")
            .key_context("pref-share-url-base")
            .track_focus(&self.pref_share_url_base_focus)
            .mt(px(8.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .bg(th.bg_hsla())
            .border_1()
            .border_color(input_border)
            .rounded(px(3.0))
            .px(px(8.0))
            .py(px(4.0))
            .min_h(px(28.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.pref_share_url_base_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_share_url_base_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char
                            && ch.chars().all(is_url_char)
                            && this.pref_share_url_base_text.len() + ch.len() <= MAX_URL_LEN
                        {
                            this.pref_share_url_base_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(share_url_base_text.is_empty(), |el| {
                el.child(
                    div()
                        .text_color(placeholder_fg)
                        .child("https://example.com/tab-atelier"),
                )
            })
            .when(!share_url_base_text.is_empty(), |el| {
                el.child(share_url_base_text)
                    .child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
            });

        let editor_text = self.pref_editor_text.clone();
        let editor_input = div()
            .id("pref-editor-input")
            .key_context("pref-editor")
            .track_focus(&self.pref_editor_focus)
            .mt(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .bg(th.bg_hsla())
            .border_1()
            .border_color(input_border)
            .rounded(px(3.0))
            .px(px(8.0))
            .py(px(4.0))
            .min_h(px(28.0))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    this.pref_editor_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_editor_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char {
                            this.pref_editor_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(editor_text.is_empty(), |el| {
                el.child(div().text_color(placeholder_fg).child(t.code_editor_placeholder))
            })
            .when(!editor_text.is_empty(), |el| {
                el.child(editor_text)
                    .child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
            });

        Some(
            div()
                .id("preferences-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(overlay_bg)
                .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, _cx| {})
                .child(
                    div()
                        .id("preferences-box")
                        .bg(modal_bg)
                        .text_color(modal_fg)
                        .border_1()
                        .border_color(modal_border)
                        .rounded(px(6.0))
                        .p(px(24.0))
                        .min_w(px(320.0))
                        .text_size(px(14.0))
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .child(div().text_size(px(16.0)).mb(px(16.0)).child(t.preferences))
                        .child(div().child(t.theme).child(theme_options))
                        .child(div().mt(px(16.0)).child(t.opacity).child(opacity_slider))
                        .child(div().mt(px(16.0)).child(t.toggle_hotkeys).child(hotkey_list))
                        .child(div().mt(px(16.0)).child(t.language).child(lang_options))
                        .child(div().mt(px(16.0)).child(t.browser).child(browser_input))
                        .child(div().mt(px(16.0)).child(t.code_editor).child(editor_input))
                        .child(div().mt(px(16.0)).child(t.api_addr).child(api_addr_input))
                        .child(div().mt(px(16.0)).child(t.api_tls_addr).child(api_tls_addr_input))
                        .child(div().mt(px(16.0)).child(t.share_url_base).child(share_url_base_input))
                        .child(
                            div()
                                .mt(px(20.0))
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .id("pref-cancel")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(option_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                                if this.show_hotkey_picker
                                                    && let Some(ref handle) = this.hotkey_handle
                                                {
                                                    handle.resume();
                                                }
                                                this.show_preferences = false;
                                                this.show_hotkey_picker = false;
                                                cx.notify();
                                            }),
                                        )
                                        .child(t.cancel),
                                )
                                .child({
                                    let ro = crate::read_only();
                                    let mut btn = div()
                                        .id("pref-save")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_bg)
                                        .rounded(px(3.0))
                                        .child(t.save);
                                    if ro {
                                        btn = btn.opacity(0.4);
                                    } else {
                                        btn = btn.cursor_pointer().hover(|s| s.bg(btn_hover)).on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                                let lang_str = match this.lang {
                                                    Lang::En => "en",
                                                    Lang::Fr => "fr",
                                                };
                                                let browser = if this.pref_browser_text.is_empty() {
                                                    None
                                                } else {
                                                    Some(this.pref_browser_text.clone())
                                                };
                                                let editor = if this.pref_editor_text.is_empty() {
                                                    None
                                                } else {
                                                    Some(this.pref_editor_text.clone())
                                                };
                                                (*this.browser.borrow_mut()).clone_from(&browser);
                                                (*this.code_editor.borrow_mut()).clone_from(&editor);
                                                // Validate each addr:port field
                                                // via `SocketAddr::parse`. Anything
                                                // that fails is kept as-is in the
                                                // edit buffer but not persisted —
                                                // the previous good value sticks.
                                                let parsed_api =
                                                    this.pref_api_addr_text.parse::<std::net::SocketAddr>().ok();
                                                if parsed_api.is_some() {
                                                    this.api_addr.clone_from(&this.pref_api_addr_text);
                                                }
                                                let parsed_tls =
                                                    this.pref_api_tls_addr_text.parse::<std::net::SocketAddr>().ok();
                                                if parsed_tls.is_some() {
                                                    this.api_tls_addr.clone_from(&this.pref_api_tls_addr_text);
                                                }
                                                // share_url_base is a free-form URL; accept whatever
                                                // the user typed (trimmed), empty means "use LAN URL".
                                                this.share_url_base = this.pref_share_url_base_text.trim().to_string();
                                                let share_url_base = if this.share_url_base.is_empty() {
                                                    None
                                                } else {
                                                    Some(this.share_url_base.clone())
                                                };
                                                let on_disk_prefs = load_preferences(&platform::config_dir());
                                                save_preferences(
                                                    &platform::config_dir(),
                                                    &Preferences {
                                                        // Font lives in preferences.json (or zed /
                                                        // fontconfig); the GUI dialog doesn't edit it,
                                                        // so carry the on-disk values through rather
                                                        // than wiping them on save.
                                                        font_family: on_disk_prefs.font_family,
                                                        font_size: on_disk_prefs.font_size,
                                                        lang: Some(lang_str.into()),
                                                        theme: Some(this.theme_name.id().into()),
                                                        opacity: Some(this.opacity),
                                                        hotkeys: this.hotkeys.clone(),
                                                        browser,
                                                        code_editor: editor,
                                                        api_addr: Some(this.api_addr.clone()),
                                                        api_tls_addr: Some(this.api_tls_addr.clone()),
                                                        // Same "advanced field, not in the GUI dialog"
                                                        // treatment as pty_cols / clear_env: the dialog
                                                        // doesn't surface a cert/key picker, so leaving
                                                        // these at None on save would silently wipe the
                                                        // operator's Cloudflare Origin cert path. The
                                                        // GUI never edits them.
                                                        api_tls_cert_path: None,
                                                        api_tls_key_path: None,
                                                        api_tls_client_ca_path: None,
                                                        share_url_base,
                                                        remote_endpoints: this.remote_endpoints.clone(),
                                                        // Headless-only fields the GUI never edits;
                                                        // preserve whatever was on disk by leaving
                                                        // them at the Default (None). The headless
                                                        // CLI (`ports --pty-cols N`) writes them
                                                        // directly into the JSON.
                                                        pty_cols: None,
                                                        pty_rows: None,
                                                        tab_bg_color: this.tab_bg_global.clone(),
                                                        // Headless-only: default allowlist for new
                                                        // tabs, set via the CLI. Preserve on-disk.
                                                        default_net_allow_presets: on_disk_prefs
                                                            .default_net_allow_presets,
                                                        default_net_allow_domains: on_disk_prefs
                                                            .default_net_allow_domains,
                                                        default_net_allow_cidrs: on_disk_prefs.default_net_allow_cidrs,
                                                        // Headless-only advanced fields set directly
                                                        // in preferences.json; not exposed in the GUI
                                                        // dialog, same treatment as pty_cols above.
                                                        default_tab_limits: crate::TabResourceLimits::default(),
                                                        clear_env: None,
                                                        clear_env_vars: std::collections::BTreeMap::new(),
                                                    },
                                                );
                                                if let Some(ref handle) = this.hotkey_handle {
                                                    handle.update_keys(&this.hotkeys);
                                                }
                                                this.show_preferences = false;
                                                this.show_hotkey_picker = false;
                                                cx.notify();
                                            }),
                                        );
                                    }
                                    btn
                                }),
                        ),
                ),
        )
    }

    fn render_hotkey_picker(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if !self.show_hotkey_picker {
            return None;
        }

        let overlay_bg = Hsla::from(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.6,
        });
        let th = self.th();
        let modal_bg = th.surface_hsla();
        let modal_fg = th.fg_hsla();
        let modal_border = th.border_hsla();
        let muted_fg = th.fg_muted_hsla();
        let error_fg = Hsla {
            h: 0.0,
            s: 0.8,
            l: 0.65,
            a: 1.0,
        };
        let t = self.t();

        Some(
            div()
                .id("hotkey-picker-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .bg(overlay_bg)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        this.show_hotkey_picker = false;
                        if let Some(ref handle) = this.hotkey_handle {
                            handle.resume();
                        }
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("hotkey-picker-box")
                        .key_context("hotkey-picker")
                        .track_focus(&self.hotkey_picker_focus)
                        .bg(modal_bg)
                        .text_color(modal_fg)
                        .border_1()
                        .border_color(modal_border)
                        .rounded(px(6.0))
                        .p(px(24.0))
                        .min_w(px(260.0))
                        .text_size(px(14.0))
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                            let key = ev.keystroke.key.as_str();
                            if key == "escape" {
                                this.show_hotkey_picker = false;
                                if let Some(ref handle) = this.hotkey_handle {
                                    handle.resume();
                                }
                                cx.notify();
                                return;
                            }
                            if let Some(kc) = gpui_key_to_keycode(key) {
                                if this.hotkeys.contains(&kc) {
                                    this.hotkey_picker_error =
                                        Some(format!("{} — {}", keycode_label(kc), t.key_already_registered));
                                } else {
                                    this.hotkeys.push(kc);
                                    this.show_hotkey_picker = false;
                                    if let Some(ref handle) = this.hotkey_handle {
                                        handle.resume();
                                    }
                                }
                                cx.notify();
                            }
                        }))
                        .child(div().text_size(px(16.0)).mb(px(8.0)).child(t.choose_a_key))
                        .child(
                            div()
                                .text_size(px(20.0))
                                .text_color(muted_fg)
                                .py(px(16.0))
                                .flex()
                                .justify_center()
                                .child(t.press_a_key),
                        )
                        .when(self.hotkey_picker_error.is_some(), |el| {
                            let err = self.hotkey_picker_error.as_deref().unwrap_or_default();
                            el.child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(error_fg)
                                    .mt(px(8.0))
                                    .flex()
                                    .justify_center()
                                    .child(err.to_string()),
                            )
                        }),
                ),
        )
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drain any tabs the API thread asked us to create. We can't call
        // insert_tab from persist() because that path doesn't have a
        // Window handle; piggy-backing on render() is the simplest place
        // to react to remote POST /tabs requests.
        let (new_tab_count, new_tab_cwds): (usize, Vec<PathBuf>) = {
            let mut snap = self.api_state.lock().unwrap();
            let n = std::mem::take(&mut snap.pending_new_tabs);
            let cwds: Vec<PathBuf> = std::mem::take(&mut snap.pending_new_tab_cwds).into_iter().collect();
            drop(snap);
            (n, cwds)
        };
        let mut cwd_iter = new_tab_cwds.into_iter();
        for _ in 0..new_tab_count {
            match cwd_iter.next() {
                Some(cwd) => self.add_tab_in(cwd, window, cx),
                None => self.add_tab(window, cx),
            }
        }
        window.set_window_title(&format!("{}{}", self.tabs[self.active].name, self.t().title_suffix));
        let active_terminal = self.tabs[self.active].view.clone();
        #[cfg(feature = "energy")]
        let battery = *self.battery_percent.lock().unwrap();
        #[cfg(not(feature = "energy"))]
        let battery: Option<u8> = None;
        let tab_bar = self.render_tab_bar(battery, window, cx);
        let context_menu = if self.renaming.is_none()
            && self.exit_confirm.is_none()
            && self.close_confirm.is_none()
            && !self.show_qr
            && !self.show_preferences
        {
            self.render_context_menu(window, cx)
        } else {
            None
        };
        let rename_input = self.render_rename_input(cx);
        let exit_confirm = self.render_exit_confirm(cx);
        let close_confirm = self.render_close_confirm(cx);
        if self.renaming.is_some() {
            self.rename_focus.focus(window);
        }
        if self.show_hotkey_picker {
            self.hotkey_picker_focus.focus(window);
        }
        // When the prefs modal is open, force focus onto one of its
        // inputs every render. Without this, the terminal's focus
        // handle (or whatever held focus before the modal opened)
        // keeps receiving KeyDownEvents and typing leaks into the
        // PTY behind the modal. The per-input on_mouse_down handlers
        // still cover switching between inputs — if focus is already
        // on a prefs input, we leave it; we only redirect to
        // api_addr when focus drifted outside the modal entirely.
        //
        // EXCEPTION: when the hotkey picker is layered on top of the
        // prefs modal, the picker has its own focus handle (anchored
        // at line ~3700 above). Forcing api_addr focus here would
        // yank focus back from the picker every frame and the user
        // could never bind a key combo — keystrokes would just hop
        // between the picker's window and api_addr at 60 Hz.
        // Anchoring is the picker's job while it's open.
        if self.show_preferences && !self.show_hotkey_picker {
            let already_in_prefs = self.pref_api_addr_focus.is_focused(window)
                || self.pref_api_tls_addr_focus.is_focused(window)
                || self.pref_share_url_base_focus.is_focused(window)
                || self.pref_browser_focus.is_focused(window)
                || self.pref_editor_focus.is_focused(window);
            if !already_in_prefs {
                self.pref_api_addr_focus.focus(window);
            }
        }

        let alpha = self.opacity as u32;
        let bg_color = if battery.is_some_and(|b| b < 10) {
            rgba((0x3a05_0500) | alpha)
        } else if battery.is_some_and(|b| b < 20) {
            rgba((0x2d08_0800) | alpha)
        } else {
            rgba((self.th().bg << 8) | alpha)
        };

        let mut root = div()
            .id("app-root")
            .size_full()
            .bg(bg_color)
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                if ks.modifiers.control && ks.modifiers.shift && ks.key.as_str() == "t" {
                    this.add_tab_after_current(window, cx);
                    return;
                }
                if ks.modifiers.alt && ks.key.as_str() == "tab" {
                    this.tabs[this.active].deactivate();
                    this.active = (this.active + 1) % this.tabs.len();
                    this.tabs[this.active].activate();
                    this.tabs[this.active].flush_pending_restore(cx);
                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id("terminal-area")
                    // Take full width but DON'T claim full height — the
                    // tab bar below uses flex-wrap to grow to 2/3 rows
                    // (32 px each) and needs space to expand into. With
                    // `size_full()` here the terminal-area pinned itself
                    // to 100% of parent height and the tab bar's 3rd row
                    // overflowed (only ~3/4 visible). `flex_grow()` is
                    // enough to absorb whatever the tab bar doesn't use.
                    .w_full()
                    .min_h(px(0.0))
                    .flex_grow()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = Some(ContextMenu {
                                kind: MenuKind::Background,
                                position: ev.position,
                                open_upward: false,
                            });
                            cx.notify();
                        }),
                    )
                    .child(active_terminal),
            )
            .child(tab_bar);

        if let Some(menu) = context_menu {
            root = root
                .child(
                    div()
                        .id("menu-overlay")
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                this.context_menu = None;
                                cx.notify();
                            }),
                        ),
                )
                .child(menu);
        }

        if let Some(rename) = rename_input {
            root = root.child(rename);
        }

        if let Some(confirm) = exit_confirm {
            root = root.child(confirm);
        }

        if let Some(confirm) = close_confirm {
            root = root.child(confirm);
        }

        if let Some(qr) = self.render_qr_modal(cx) {
            root = root.child(qr);
        }

        if let Some(prefs) = self.render_preferences(cx) {
            root = root.child(prefs);
        }

        if let Some(picker) = self.render_hotkey_picker(cx) {
            root = root.child(picker);
        }

        if !self.toasts.is_empty() {
            let th = self.th();
            let toast_bg = th.elevated_hsla();
            let toast_fg = th.fg_hsla();
            let toast_border = th.accent_hsla();
            let link_fg = th.accent_hsla();
            let mut stack = div()
                .id("toast-stack")
                .absolute()
                .bottom(px(48.0))
                .right(px(16.0))
                .flex()
                .flex_col()
                .gap(px(6.0));
            for (i, toast) in self.toasts.iter().enumerate() {
                let path_clone = toast.path.clone();
                let mut el = div()
                    .id(SharedString::from(format!("toast-{i}")))
                    .bg(toast_bg)
                    .text_color(toast_fg)
                    .border_1()
                    .border_color(toast_border)
                    .rounded(px(6.0))
                    .px(px(16.0))
                    .py(px(10.0))
                    .text_size(px(13.0));
                if let Some(ref path) = toast.path {
                    el = el
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(format!("{}:", toast.message))
                        .child(div().text_color(link_fg).child(path.display().to_string()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _ev: &MouseDownEvent, _window, _cx| {
                                if let Some(ref path) = path_clone {
                                    platform::open_path(path, None);
                                }
                            }),
                        );
                } else {
                    el = el.child(toast.message.clone());
                }
                stack = stack.child(el);
            }
            root = root.child(stack);
        }

        root
    }
}

// Shared with the headless binary — see `crate::tab_env_extras`,
// `crate::api_url_for_local_clients`, and
// `crate::build_agent_resume_command` in lib.rs.

/// Pull the port out of an `addr:port` bind string. Falls back to
/// `fallback` when the string is malformed (covers IPv4, IPv6 like
/// `[::1]:N`, and bare `:N`).
fn port_of(bind: &str, fallback: u16) -> u16 {
    bind.rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(fallback)
}

/// `addr:port` is a small, well-bounded ASCII subset (digits, dots,
/// colons, brackets, hex letters for IPv6). Anything else is junk and
/// we refuse to insert it so the `SocketAddr` parse on Save can't fail
/// in subtle ways.
fn is_addr_port_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '.' | ':' | '[' | ']') || ('a'..='f').contains(&c.to_ascii_lowercase())
}

const MAX_ADDR_LEN: usize = 64;

/// Char predicate for the share-URL-base input — accepts the URL-safe
/// ASCII set (RFC 3986 reserved + unreserved + a few practical extras
/// like spaces / `?` not really allowed but tolerated for paste).
const fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            ':' | '/'
                | '.'
                | '-'
                | '_'
                | '~'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

const MAX_URL_LEN: usize = 256;

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

fn run_check() {
    println!("tab-atelier v{} --check", env!("CARGO_PKG_VERSION"));

    let libs: &[(&str, &str)] = &[
        ("libfreetype.so.6", "libfreetype6"),
        ("libxkbcommon.so.0", "libxkbcommon0"),
        ("libxkbcommon-x11.so.0", "libxkbcommon-x11-0"),
        ("libxcb.so.1", "libxcb1"),
        ("libxcb-xkb.so.1", "libxcb-xkb1"),
    ];
    let mut ok = true;
    let mut missing = Vec::new();
    for (lib, pkg) in libs {
        print!("  {lib:<30}");
        let found = std::path::Path::new("/usr/lib/x86_64-linux-gnu").join(lib).exists()
            || std::path::Path::new("/usr/lib64").join(lib).exists()
            || std::path::Path::new("/usr/lib").join(lib).exists();
        if found {
            println!("ok");
        } else {
            println!("MISSING  (apt install {pkg})");
            missing.push(*pkg);
            ok = false;
        }
    }

    print!("  /dev/ptmx (pty support) ..... ");
    if std::path::Path::new("/dev/ptmx").exists() {
        println!("ok");
    } else {
        println!("MISSING");
        ok = false;
    }

    let state_dir = platform::state_base_dir();
    print!("  state dir ................... ");
    println!("{}", state_dir.display());

    let config_dir = platform::config_dir();
    print!("  config dir .................. ");
    println!("{}", config_dir.display());

    if ok {
        println!("all checks passed");
    } else {
        println!("\nTo fix, run:\n  sudo apt install {}", missing.join(" "));
        std::process::exit(1);
    }
}

/// Launch the gpui application. Blocks until the window closes.
///
/// # Panics
/// Panics if gpui fails to open its initial window (e.g. no X server).
pub fn run() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        run_check();
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tab-atelier v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    info!("starting Tab Atelier v{}", env!("CARGO_PKG_VERSION"));
    Application::new().run(|cx: &mut App| {
        let prefs = load_preferences(&platform::config_dir());
        let keycodes: Vec<u8> = if prefs.hotkeys.is_empty() {
            DEFAULT_HOTKEYS.to_vec()
        } else {
            prefs.hotkeys
        };

        let window_handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_background: WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |window, cx| {
                    window.toggle_fullscreen();
                    cx.new(|cx| AppState::new(window, cx))
                },
            )
            .unwrap();

        spawn_hotkey_listener(&keycodes, window_handle, cx);
    });
}

fn spawn_hotkey_listener(keycodes: &[u8], window_handle: WindowHandle<AppState>, cx: &mut App) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();

    let handle = platform::grab_hotkeys(keycodes, move || {
        let _ = tx.send(());
    });

    let _ = window_handle.update(cx, |state, _window, _cx| {
        state.hotkey_handle = Some(handle);
    });

    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(50))
                .await;
            if rx.try_recv().is_ok() {
                let _ = cx.update(|cx| {
                    let _ = window_handle.update(cx, |state, window, _cx| {
                        state.visible = !state.visible;
                        if state.visible {
                            state.tabs[state.active].activate();
                            window.activate_window();
                        } else {
                            state.tabs[state.active].deactivate();
                            window.minimize_window();
                        }
                    });
                });
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(std::time::Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(std::time::Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(std::time::Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(std::time::Duration::from_mins(1)), "1m 0s");
        assert_eq!(format_duration(std::time::Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(std::time::Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(std::time::Duration::from_hours(1)), "1h 0m");
        assert_eq!(format_duration(std::time::Duration::from_mins(121)), "2h 1m");
        assert_eq!(format_duration(std::time::Duration::from_hours(24)), "24h 0m");
    }
}
