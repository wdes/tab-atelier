// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::api;
use crate::locale::{self, Lang, Strings};
use crate::platform;
#[cfg(feature = "energy")]
use crate::power;
use crate::screenshot;
use crate::terminal::TerminalView;
use crate::theme::{self, ThemeName};
use crate::tracking::WakatimeTracker;
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
use tab_atelier::{
    DEFAULT_HOTKEYS, FontConfig, Preferences, SavedState, TabState, gpui_key_to_keycode, keycode_label,
    load_font_config, load_preferences, load_state_with_outputs, load_wakatime_key, save_preferences, save_state,
    save_tab_energy, save_tab_output, save_tab_uptime,
};

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
    /// Saved scrollback that hasn't been fed back into the terminal yet.
    /// Tabs other than the active one defer this work until first focus
    /// so cold-launch with many tabs doesn't block on vte-parsing each
    /// one's entire history up front.
    pending_restore: Option<String>,
}

impl Tab {
    /// Wall-clock time this tab has existed (live run + persisted prior
    /// runs). The mobile remote's per-tab counter reads from this, so it
    /// keeps ticking even when no input is happening — the "actively
    /// typing" semantic of the old `active_duration` field surprised
    /// users who only viewed tabs from the phone.
    fn uptime(&self) -> std::time::Duration {
        self.prior_uptime + self.created_at.elapsed()
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
    /// HTTP API port (TLS uses port + 1). Sourced from the saved
    /// preference at startup; live changes require a restart since the
    /// `TcpListener`s are bound in spawned threads.
    api_port: u16,
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
    /// Editable copy of `api_port` shown in the preferences dialog.
    /// Persisted to `api_port` only on Save and applied on next launch
    /// (the API listener threads bind once at startup).
    pref_api_port_text: String,
    pref_api_port_focus: FocusHandle,
    hotkey_handle: Option<platform::HotkeyHandle>,
    /// When the per-tab uptime files were last written. Persisting uptime
    /// every 2s would burn through disk writes for a value that only
    /// advances by ~2s anyway; we batch writes to once every 30s.
    last_uptime_save: std::cell::Cell<Option<std::time::Instant>>,
    /// CRC32 of the last serialized `tabs.json` content. Skips the write+
    /// rotate when nothing in the tab list changed since last tick.
    last_state_hash: std::cell::Cell<u32>,
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
        let pref_api_port_focus = cx.focus_handle();
        let font_config = load_font_config(&platform::config_dir());
        let prefs = load_preferences(&platform::config_dir());
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
                    let view = cx.new(|cx| {
                        let mut tv = TerminalView::new_with_colors(cwd.as_deref(), fc, br, ce, colors, window, cx);
                        tv.theme = theme_name;
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
                    tabs.push(Tab {
                        view,
                        name: ts.name.clone(),
                        created_at: std::time::Instant::now(),
                        prior_uptime: std::time::Duration::from_secs_f64(ts.uptime_secs.unwrap_or(0.0)),
                        active_duration: std::time::Duration::ZERO,
                        last_activated: None,
                        #[cfg(feature = "energy")]
                        energy_wh: ts.energy_wh.unwrap_or(0.0),
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: ts.energy_wh.unwrap_or(0.0),
                        // Seed with the hash of the just-restored output so the
                        // first persist tick after launch doesn't rewrite an
                        // identical file.
                        output_hash_last_saved: ts.output.as_deref().map_or(0, |s| tab_atelier::crc32(s.as_bytes())),
                        pending_restore,
                    });
                }
                if tabs.is_empty() {
                    let fc = font_config.clone();
                    let br = browser.clone();
                    let ce = code_editor.clone();
                    let view = cx.new(|cx| {
                        let mut tv = TerminalView::new(None, fc, br, ce, window, cx);
                        tv.theme = theme_name;
                        tv
                    });
                    tabs.push(Tab {
                        view,
                        name: locale::strings(lang).terminal.into(),
                        created_at: std::time::Instant::now(),
                        prior_uptime: std::time::Duration::ZERO,
                        active_duration: std::time::Duration::ZERO,
                        last_activated: None,
                        #[cfg(feature = "energy")]
                        energy_wh: 0.0,
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: 0.0,
                        output_hash_last_saved: 0,
                        pending_restore: None,
                    });
                }
                let active = saved.active.min(tabs.len() - 1);
                tabs[active].activate();
                (tabs, active, saved.windowed)
            } else {
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let view = cx.new(|cx| {
                    let mut tv = TerminalView::new(None, fc, br, ce, window, cx);
                    tv.theme = theme_name;
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
                        #[cfg(feature = "energy")]
                        energy_wh: 0.0,
                        #[cfg(feature = "energy")]
                        energy_wh_last_saved: 0.0,
                        output_hash_last_saved: 0,
                        pending_restore: None,
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

        let tracker = load_wakatime_key(&platform::config_dir()).map(|key| {
            info!("wakatime tracking enabled");
            WakatimeTracker::new(key)
        });

        let api_token = api::load_or_generate_token();
        let api_port = prefs.api_port.unwrap_or(tab_atelier::DEFAULT_API_PORT);
        info!("API server starting on 0.0.0.0:{api_port}");
        let api_state = Arc::new(Mutex::new(api::TabSnapshot {
            tabs: Vec::<api::SnapshotTab>::new(),
            active: 0,
            #[cfg(feature = "energy")]
            power: Vec::new(),
            #[cfg(feature = "energy")]
            battery_percent: None,
            pending_closes: Vec::new(),
            pending_activate: None,
            pending_input: Vec::new(),
            pending_new_tabs: 0,
            pending_renames: Vec::new(),
        }));
        let api_read_only = crate::read_only();
        api::start_api_server(api_state.clone(), api_token.clone(), api_read_only, api_port);
        api::start_api_server_tls(api_state.clone(), api_token.clone(), api_read_only, api_port + 1);

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
            api_port,
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
            pref_api_port_text: String::new(),
            pref_api_port_focus,
            hotkey_handle: None,
            last_uptime_save: std::cell::Cell::new(None),
            last_state_hash: std::cell::Cell::new(0),
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.tabs.len(), window, cx);
    }

    fn add_tab_after_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.active + 1, window, cx);
    }

    fn insert_tab(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = {
            let pid = self.tabs[self.active].view.read(cx).pid();
            platform::process_cwd(pid)
        };
        self.tabs[self.active].deactivate();
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let tn = self.theme_name;
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new(cwd.as_deref(), fc, br, ce, window, cx);
            tv.theme = tn;
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
                #[cfg(feature = "energy")]
                energy_wh: 0.0,
                #[cfg(feature = "energy")]
                energy_wh_last_saved: 0.0,
                output_hash_last_saved: 0,
                pending_restore: None,
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
        #[allow(clippy::needless_collect)]
        let outputs: Vec<(String, String)> = self
            .tabs
            .iter()
            .map(|tab| (tab.name.clone(), tab.view.read(cx).copy_all_history()))
            .collect();
        let uptimes: Vec<(String, f64)> = self
            .tabs
            .iter()
            .map(|tab| (tab.name.clone(), tab.uptime().as_secs_f64()))
            .collect();
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let pid = tab.view.read(cx).pid();
                let cwd = platform::process_cwd(pid).map(|p| p.to_string_lossy().into_owned());
                TabState {
                    name: tab.name.clone(),
                    cwd,
                    // Output and uptime are now persisted in per-tab files.
                    output: None,
                    uptime_secs: None,
                    #[cfg(feature = "energy")]
                    energy_wh: None,
                    #[cfg(not(feature = "energy"))]
                    energy_wh: None,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                }
            })
            .collect();
        let api_tabs: Vec<api::SnapshotTab> = self
            .tabs
            .iter()
            .zip(tabs.iter())
            .map(|(tab, ts)| {
                let view = tab.view.read(cx);
                let (output, cursor) = view.ansi_text_with_cursor(Some(200));
                api::SnapshotTab {
                    name: ts.name.clone(),
                    cwd: ts.cwd.clone(),
                    // Cache 200 lines so remote clients can request scrollback.
                    // ANSI escapes are kept so the mobile remote can render
                    // colours instead of the previous flat-grey text.
                    output,
                    uptime_secs: tab.uptime().as_secs_f64(),
                    cursor,
                    shell_pid: view.pid(),
                }
            })
            .collect();

        let read_only = crate::read_only();
        let saved = SavedState {
            tabs,
            active: self.active,
            windowed: self.windowed,
        };
        // Skip the write+rotate when the serialized content is identical to
        // last tick — the common case once the user stops poking the UI.
        let serialized = serde_json::to_string_pretty(&saved).unwrap_or_default();
        let new_hash = tab_atelier::crc32(serialized.as_bytes());
        if !read_only && new_hash != self.last_state_hash.get() {
            save_state(&platform::config_base_dir(), &saved);
            self.last_state_hash.set(new_hash);
        }
        if !read_only {
            for (i, (name, output)) in outputs.into_iter().enumerate() {
                if output.is_empty() {
                    continue;
                }
                let h = tab_atelier::crc32(output.as_bytes());
                if h == self.tabs[i].output_hash_last_saved {
                    continue;
                }
                save_tab_output(&state_base, &name, &output);
                self.tabs[i].output_hash_last_saved = h;
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
                for (name, secs) in &uptimes {
                    save_tab_uptime(&state_base, name, *secs);
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
            drop(snapshot);
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
            let pid = self.tabs[self.active].view.read(cx).pid();
            let cwd = platform::process_cwd(pid);
            tracker.record_activity(cwd);
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
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new(cwd.as_deref(), fc, br, ce, window, cx);
            tv.theme = tn;
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
                tab_atelier::tab_output_path as fn(&std::path::Path, &str) -> std::path::PathBuf,
                tab_atelier::tab_uptime_path,
                tab_atelier::tab_power_path,
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
        #[allow(clippy::needless_collect)]
        let outputs: Vec<(String, String)> = self
            .tabs
            .iter()
            .map(|tab| (tab.name.clone(), tab.view.read(cx).copy_all_history()))
            .collect();
        let uptimes: Vec<(String, f64)> = self
            .tabs
            .iter()
            .map(|tab| (tab.name.clone(), tab.uptime().as_secs_f64()))
            .collect();
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let pid = tab.view.read(cx).pid();
                let cwd = platform::process_cwd(pid).map(|p| p.to_string_lossy().into_owned());
                TabState {
                    name: tab.name.clone(),
                    cwd,
                    output: None,
                    uptime_secs: None,
                    #[cfg(feature = "energy")]
                    energy_wh: None,
                    #[cfg(not(feature = "energy"))]
                    energy_wh: None,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
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
            for (name, output) in outputs {
                if !output.is_empty() {
                    save_tab_output(&state_base, &name, &output);
                }
            }
            // Always flush uptime + energy on shutdown — bypass throttles so
            // the last tick isn't lost.
            for (name, secs) in &uptimes {
                save_tab_uptime(&state_base, name, *secs);
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

        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            let name = if let Some((ri, ref text)) = self.renaming {
                if ri == i { text.clone() } else { tab.name.clone() }
            } else {
                tab.name.clone()
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
                // Left border so the first tab of each wrapped row
                // shows a separator on its left edge — right-only
                // borders left the bar looking unbounded at every
                // row start.
                .border_l_1()
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

            // Switch the active shell into a catbus session by exec'ing
            // catbus-agent inside the existing PTY. `exec` replaces the
            // shell so the tab's PID stays the same and the existing
            // session-discovery walker finds the new process by `comm`.
            // The U+FE0F variation selectors after each emoji nudge
            // font fallback toward the colour-emoji face for both
            // glyphs, which on Linux otherwise picks different fonts.
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
                            let view = &this.tabs[idx].view;
                            // Ctrl-L clears the screen without spawning a subprocess,
                            // then exec replaces the shell with catbus-agent in-place.
                            view.read(cx).send_input_bytes(vec![0x0c]);
                            view.read(cx).send_clipboard("exec catbus-agent\n");
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{1f408}\u{fe0f}\u{1f68c}\u{fe0f} Catbus"),
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
                            let text = this.tabs[this.active].view.read(cx).copy_all_history();
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_all),
            )
            .child(
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
            )
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
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.pref_browser_text = this.browser.borrow().clone().unwrap_or_default();
                            this.pref_editor_text = this.code_editor.borrow().clone().unwrap_or_default();
                            this.pref_api_port_text = this.api_port.to_string();
                            this.show_preferences = true;
                            this.context_menu = None;
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
        let lan_url = format!("http://{primary_ip}:{}", self.api_port);
        let lan_url_tls = format!("https://{primary_ip}:{}", self.api_port + 1);
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
                                list = list.child(
                                    div()
                                        .text_color(link_fg)
                                        .child(format!("http://{ip}:{}", self.api_port)),
                                );
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
                                tab.view.update(cx, |tv, _cx| tv.theme = tn);
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

        let api_port_text = self.pref_api_port_text.clone();
        let api_port_input = div()
            .id("pref-api-port-input")
            .key_context("pref-api-port")
            .track_focus(&self.pref_api_port_focus)
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
                    this.pref_api_port_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_api_port_text.pop();
                        cx.notify();
                    }
                    _ => {
                        // Numeric only so the parsed `u16` on Save can't
                        // silently drop junk the user typed.
                        if let Some(ref ch) = ev.keystroke.key_char
                            && ch.chars().all(|c| c.is_ascii_digit())
                            && this.pref_api_port_text.len() < 5
                        {
                            this.pref_api_port_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(api_port_text.is_empty(), |el| {
                el.child(
                    div()
                        .text_color(placeholder_fg)
                        .child(tab_atelier::DEFAULT_API_PORT.to_string()),
                )
            })
            .when(!api_port_text.is_empty(), |el| {
                el.child(api_port_text)
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
                        .child(div().mt(px(16.0)).child(t.api_port).child(api_port_input))
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
                                                // Parse the port field. Anything
                                                // that fails sanity-checks (empty,
                                                // non-numeric, > 65534) falls back
                                                // to whatever was already loaded.
                                                let parsed_port = this
                                                    .pref_api_port_text
                                                    .parse::<u16>()
                                                    .ok()
                                                    .filter(|&p| p > 0 && p < u16::MAX);
                                                if let Some(p) = parsed_port {
                                                    this.api_port = p;
                                                }
                                                save_preferences(
                                                    &platform::config_dir(),
                                                    &Preferences {
                                                        lang: Some(lang_str.into()),
                                                        theme: Some(this.theme_name.id().into()),
                                                        opacity: Some(this.opacity),
                                                        hotkeys: this.hotkeys.clone(),
                                                        browser,
                                                        code_editor: editor,
                                                        api_port: Some(this.api_port),
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
        let new_tab_count = {
            let mut snap = self.api_state.lock().unwrap();
            std::mem::take(&mut snap.pending_new_tabs)
        };
        for _ in 0..new_tab_count {
            self.add_tab(window, cx);
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
                    .flex_grow()
                    .size_full()
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
