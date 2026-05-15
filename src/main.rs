// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod locale;
mod platform;
mod power;
mod screenshot;
mod terminal;
mod terminal_utils;
mod tracking;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Application, AsyncApp, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowHandle, WindowOptions, div, px, rgb, rgba,
};
use locale::{Lang, Strings};
use log::{debug, info};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tab_atelier::{
    FontConfig, Preferences, SavedState, TabState, load_font_config, load_preferences, load_state_from,
    load_wakatime_key, save_preferences, save_state,
};
use terminal::TerminalView;
use tracking::WakatimeTracker;

struct Tab {
    view: Entity<TerminalView>,
    name: String,
    active_duration: std::time::Duration,
    last_activated: Option<std::time::Instant>,
    energy_wh: f64,
}

impl Tab {
    fn uptime(&self) -> std::time::Duration {
        self.active_duration + self.last_activated.map_or(std::time::Duration::ZERO, |t| t.elapsed())
    }

    fn activate(&mut self) {
        if self.last_activated.is_none() {
            self.last_activated = Some(std::time::Instant::now());
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
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(12.0))
            .py(px(4.0))
            .bg(rgb(0x2d_2d2d))
            .text_color(rgb(0xcc_cccc))
            .text_size(px(13.0))
            .rounded(px(4.0))
            .opacity(0.8)
            .child(self.name.clone())
    }
}

struct ExitConfirm {
    tab_idx: usize,
}

struct Swoop {
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
    api_state: Arc<Mutex<api::TabSnapshot>>,
    power_pids: Arc<Mutex<Vec<u32>>>,
    power_watts: Arc<Mutex<Vec<power::TabPower>>>,
    battery_percent: Arc<Mutex<Option<u8>>>,
    blink_on: bool,
    toasts: Vec<Toast>,
    lang: Lang,
    show_preferences: bool,
    browser: Rc<RefCell<Option<String>>>,
    code_editor: Rc<RefCell<Option<String>>>,
    pref_browser_text: String,
    pref_browser_focus: FocusHandle,
    pref_editor_text: String,
    pref_editor_focus: FocusHandle,
}

impl Swoop {
    fn t(&self) -> &'static Strings {
        locale::strings(self.lang)
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename_focus = cx.focus_handle();
        let pref_browser_focus = cx.focus_handle();
        let pref_editor_focus = cx.focus_handle();
        let font_config = load_font_config(&platform::config_dir());
        let prefs = load_preferences(&platform::state_base_dir());
        let browser: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.browser.clone()));
        let code_editor: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.code_editor.clone()));
        let lang = match prefs.lang.as_deref() {
            Some("fr") => Lang::Fr,
            Some("en") => Lang::En,
            _ => locale::detect_lang(),
        };

        let (tabs, active) = if let Some(saved) = load_state_from(&platform::state_base_dir()) {
            info!("restoring {} tab(s) from saved state", saved.tabs.len());
            let mut tabs = Vec::new();
            for ts in &saved.tabs {
                let cwd = ts.cwd.as_ref().map(PathBuf::from);
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, br, ce, window, cx));
                if let Some(ref output) = ts.output {
                    debug!("restoring {} chars of output for '{}'", output.len(), ts.name);
                    view.read(cx).restore_output(output);
                }
                tabs.push(Tab {
                    view,
                    name: ts.name.clone(),
                    active_duration: std::time::Duration::from_secs_f64(ts.uptime_secs.unwrap_or(0.0)),
                    last_activated: None,
                    energy_wh: ts.energy_wh.unwrap_or(0.0),
                });
            }
            if tabs.is_empty() {
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let view = cx.new(|cx| TerminalView::new(None, fc, br, ce, window, cx));
                tabs.push(Tab {
                    view,
                    name: locale::strings(lang).terminal.into(),
                    active_duration: std::time::Duration::ZERO,
                    last_activated: None,
                    energy_wh: 0.0,
                });
            }
            let active = saved.active.min(tabs.len() - 1);
            tabs[active].activate();
            (tabs, active)
        } else {
            let fc = font_config.clone();
            let br = browser.clone();
            let ce = code_editor.clone();
            let view = cx.new(|cx| TerminalView::new(None, fc, br, ce, window, cx));
            (
                vec![Tab {
                    view,
                    name: locale::strings(lang).terminal.into(),
                    active_duration: std::time::Duration::ZERO,
                    last_activated: Some(std::time::Instant::now()),
                    energy_wh: 0.0,
                }],
                0,
            )
        };

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

        let api_token = api::generate_token();
        info!("API server starting on 0.0.0.0:7890");
        let api_state = Arc::new(Mutex::new(api::TabSnapshot {
            tabs: Vec::new(),
            active: 0,
            power: Vec::new(),
            pending_closes: Vec::new(),
        }));
        api::start_api_server(api_state.clone(), api_token.clone());

        let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let power_watts: Arc<Mutex<Vec<power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
        let battery_percent: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));
        power::start_power_monitor(power_pids.clone(), power_watts.clone(), battery_percent.clone());

        Self {
            tabs,
            active,
            context_menu: None,
            renaming: None,
            rename_select_all: false,
            rename_focus,
            visible: true,
            windowed: false,
            exit_confirm: None,
            close_confirm: None,
            show_qr: false,
            font_config,
            tracker,
            api_token,
            api_state,
            power_pids,
            power_watts,
            battery_percent,
            blink_on: false,
            toasts: Vec::new(),
            lang,
            show_preferences: false,
            browser,
            code_editor,
            pref_browser_text: String::new(),
            pref_browser_focus,
            pref_editor_text: String::new(),
            pref_editor_focus,
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = {
            let pid = self.tabs[self.active].view.read(cx).pid();
            platform::process_cwd(pid)
        };
        self.tabs[self.active].deactivate();
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, br, ce, window, cx));
        let idx = self.tabs.len();
        self.tabs.push(Tab {
            view,
            name: format!("{} {}", self.t().terminal_n, idx + 1),
            active_duration: std::time::Duration::ZERO,
            last_activated: Some(std::time::Instant::now()),
            energy_wh: 0.0,
        });
        self.active = idx;
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
        {
            let watts = self.power_watts.lock().unwrap();
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if let Some(w) = watts.get(i).and_then(|p| p.watts) {
                    tab.energy_wh += w * 2.0 / 3600.0;
                }
            }
        }
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
                    uptime_secs: Some(tab.uptime().as_secs_f64()),
                    energy_wh: if tab.energy_wh > 0.0 { Some(tab.energy_wh) } else { None },
                }
            })
            .collect();
        let api_tabs: Vec<(String, Option<String>)> = tabs.iter().map(|t| (t.name.clone(), t.cwd.clone())).collect();

        save_state(
            &platform::state_base_dir(),
            &SavedState {
                tabs,
                active: self.active,
            },
        );

        {
            let mut snapshot = self.api_state.lock().unwrap();
            snapshot.tabs = api_tabs;
            snapshot.active = self.active;
            snapshot.power.clone_from(&self.power_watts.lock().unwrap());
        }

        {
            let pids: Vec<u32> = self.tabs.iter().map(|tab| tab.view.read(cx).pid()).collect();
            *self.power_pids.lock().unwrap() = pids;
        }

        {
            let mut snapshot = self.api_state.lock().unwrap();
            let mut closes: Vec<usize> = snapshot.pending_closes.drain(..).collect();
            drop(snapshot);
            closes.sort_unstable();
            closes.dedup();
            for idx in closes.into_iter().rev() {
                if idx < self.tabs.len() && self.tabs.len() > 1 {
                    self.close_tab(idx, cx);
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
        let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, br, ce, window, cx));
        self.tabs[idx].view = view;
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        self.tabs[idx].energy_wh = 0.0;
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
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        self.tabs[idx].energy_wh = 0.0;
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let pid = tab.view.read(cx).pid();
                let cwd = platform::process_cwd(pid).map(|p| p.to_string_lossy().into_owned());
                let output = {
                    let text = tab.view.read(cx).copy_all_history();
                    if text.is_empty() { None } else { Some(text) }
                };
                TabState {
                    name: tab.name.clone(),
                    cwd,
                    output,
                    uptime_secs: Some(tab.uptime().as_secs_f64()),
                    energy_wh: if tab.energy_wh > 0.0 { Some(tab.energy_wh) } else { None },
                }
            })
            .collect();
        save_state(
            &platform::state_base_dir(),
            &SavedState {
                tabs,
                active: self.active,
            },
        );

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
            let render_time = std::time::Instant::now();
            let _ = this.update(cx, |state, cx| {
                state.toasts.retain(|t| t.time != progress_time);
                state.toasts.push(Toast {
                    message: state.t().rendering_screenshot.into(),
                    time: render_time,
                    path: None,
                });
                cx.notify();
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(50))
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
                state.toasts.retain(|t| t.time != render_time);
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

    fn render_tab_bar(&mut self, battery: Option<u8>, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let battery_critical = battery.is_some_and(|b| b < 10);
        let blink_red = battery_critical && self.blink_on;

        let tab_bg: Hsla = rgb(0x1e_1e1e).into();
        let tab_active_bg: Hsla = rgb(0x2d_2d2d).into();
        let tab_blink_bg: Hsla = rgb(0x5c_1010).into();
        let tab_fg: Hsla = rgb(0xcc_cccc).into();
        let tab_border: Hsla = rgb(0x3c_3c3c).into();
        let watts_fg: Hsla = rgb(0x88_8888).into();

        let watts = self.power_watts.lock().unwrap().clone();

        let mut bar = div()
            .flex()
            .flex_row()
            .w_full()
            .h(px(32.0))
            .bg(tab_bg)
            .border_t_1()
            .border_color(tab_border);

        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            let name = if let Some((ri, ref text)) = self.renaming {
                if ri == i { text.clone() } else { tab.name.clone() }
            } else {
                tab.name.clone()
            };

            let power_label = watts.get(i).map(power::TabPower::label).unwrap_or_default();

            let drag_name = tab.name.clone();
            let tab_el = div()
                .id(ElementId::Name(format!("tab-{i}").into()))
                .flex()
                .items_center()
                .px(px(12.0))
                .h_full()
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
                    if ev.click_count() == 2 {
                        let name = this.tabs[i].name.clone();
                        this.renaming = Some((i, name));
                        this.rename_select_all = true;
                        this.rename_focus.focus(window);
                    } else {
                        this.tabs[this.active].deactivate();
                        this.active = i;
                        this.tabs[i].activate();
                        this.context_menu = None;
                        this.tabs[i].view.read(cx).focus_handle(cx).focus(window);
                    }
                    cx.notify();
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
                    },
                    |tab, _offset, _window, cx| cx.new(|_| tab.clone()),
                )
                .drag_over::<DraggedTab>(move |style, dragged, _window, _cx| {
                    if dragged.idx == i {
                        return style;
                    }
                    let s = style.bg(rgb(0x09_4771));
                    if i < dragged.idx {
                        s.border_l_2()
                    } else {
                        s.border_r_2()
                    }
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.move_tab(dragged.idx, i, window, cx);
                }))
                .child(name)
                .child(
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
            .w(px(32.0))
            .h_full()
            .text_color(tab_fg)
            .text_size(px(18.0))
            .cursor_pointer()
            .hover(|s| s.bg(tab_active_bg))
            .on_click(cx.listener(|this, _ev: &ClickEvent, window, cx| {
                this.add_tab(window, cx);
            }))
            .child("+");

        bar.child(plus_btn)
    }

    fn render_context_menu(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let menu = self.context_menu.as_ref()?;
        let menu_bg: Hsla = rgb(0x25_2526).into();
        let menu_fg: Hsla = rgb(0xcc_cccc).into();
        let menu_hover: Hsla = rgb(0x09_4771).into();
        let menu_border: Hsla = rgb(0x3c_3c3c).into();

        let pos = menu.position;

        let mut container = div().id("context-menu").absolute().left(pos.x);

        container = if menu.open_upward {
            container.bottom(px(0.0))
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
        }

        {
            let stats_idx = match menu.kind {
                MenuKind::Tab(idx) => idx,
                MenuKind::Background => self.active,
            };
            let stat_fg: Hsla = rgb(0x88_8888).into();
            let elapsed = self.tabs[stats_idx].uptime();
            let power_info = self.power_watts.lock().unwrap().get(stats_idx).cloned();
            let t = self.t();

            let mut stats_lines: Vec<String> = Vec::new();

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
                                && let Some(text) = item.text()
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
        let input_bg: Hsla = rgb(0x1e_1e1e).into();
        let input_fg: Hsla = rgb(0xcc_cccc).into();
        let input_border: Hsla = rgb(0x00_7acc).into();
        let cursor_color: Hsla = rgb(0xcc_cccc).into();

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
                                        this.tabs[i].name = text.clone();
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
                                .bg(rgb(0x14_1414))
                                .border_1()
                                .border_color(input_border)
                                .rounded(px(3.0))
                                .px(px(8.0))
                                .py(px(4.0))
                                .min_h(px(28.0))
                                .cursor_text()
                                .when(self.rename_select_all, |el| {
                                    el.child(div().bg(rgb(0x26_4f78)).px(px(2.0)).child(text.clone()))
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

        let dialog_bg: Hsla = rgb(0x25_2526).into();
        let dialog_fg: Hsla = rgb(0xcc_cccc).into();
        let dialog_border: Hsla = rgb(0x3c_3c3c).into();
        let btn_bg: Hsla = rgb(0x0e_639c).into();
        let btn_hover: Hsla = rgb(0x11_77bb).into();
        let btn_secondary_bg: Hsla = rgb(0x3c_3c3c).into();
        let btn_secondary_hover: Hsla = rgb(0x50_5050).into();

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
                                .text_color(rgb(0x99_9999))
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
                                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                                this.exit_confirm = None;
                                                if this.tabs.len() <= 1 {
                                                    this.close_all_tabs(cx);
                                                } else {
                                                    this.close_tab(idx, cx);
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

        let dialog_bg: Hsla = rgb(0x25_2526).into();
        let dialog_fg: Hsla = rgb(0xcc_cccc).into();
        let dialog_border: Hsla = rgb(0x3c_3c3c).into();
        let btn_bg: Hsla = rgb(0x0e_639c).into();
        let btn_hover: Hsla = rgb(0x11_77bb).into();
        let btn_secondary_bg: Hsla = rgb(0x3c_3c3c).into();
        let btn_secondary_hover: Hsla = rgb(0x50_5050).into();

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

        let ip = api::local_ip();
        let url = format!("http://{}:7890?token={}", ip, self.api_token);
        let url_for_click = url.clone();

        let Ok(qr) = qrcode::QrCode::new(url.as_bytes()) else {
            return None;
        };

        let dialog_bg: Hsla = rgb(0x25_2526).into();
        let dialog_fg: Hsla = rgb(0xcc_cccc).into();
        let dialog_border: Hsla = rgb(0x3c_3c3c).into();
        let btn_bg: Hsla = rgb(0x0e_639c).into();
        let btn_hover: Hsla = rgb(0x11_77bb).into();
        let link_fg: Hsla = rgb(0x37_94ff).into();

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
        let modal_bg: Hsla = rgb(0x1e_1e1e).into();
        let modal_fg: Hsla = rgb(0xcc_cccc).into();
        let modal_border: Hsla = rgb(0x3c_3c3c).into();
        let input_border: Hsla = rgb(0x00_7acc).into();
        let btn_bg: Hsla = rgb(0x00_7acc).into();
        let btn_hover: Hsla = rgb(0x1c_8cd9).into();
        let option_bg: Hsla = rgb(0x2d_2d2d).into();
        let option_active: Hsla = rgb(0x00_7acc).into();
        let placeholder_fg: Hsla = rgb(0x66_6666).into();
        let cursor_color: Hsla = rgb(0xcc_cccc).into();
        let t = self.t();

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
            .bg(rgb(0x14_1414))
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

        let editor_text = self.pref_editor_text.clone();
        let editor_input = div()
            .id("pref-editor-input")
            .key_context("pref-editor")
            .track_focus(&self.pref_editor_focus)
            .mt(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(0x14_1414))
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
                        .child(div().child(t.language).child(lang_options))
                        .child(div().mt(px(16.0)).child(t.browser).child(browser_input))
                        .child(div().mt(px(16.0)).child(t.code_editor).child(editor_input))
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
                                                this.show_preferences = false;
                                                cx.notify();
                                            }),
                                        )
                                        .child(t.cancel),
                                )
                                .child(
                                    div()
                                        .id("pref-save")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_hover))
                                        .on_mouse_down(
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
                                                save_preferences(
                                                    &platform::state_base_dir(),
                                                    &Preferences {
                                                        lang: Some(lang_str.into()),
                                                        browser,
                                                        code_editor: editor,
                                                    },
                                                );
                                                this.show_preferences = false;
                                                cx.notify();
                                            }),
                                        )
                                        .child(t.save),
                                ),
                        ),
                ),
        )
    }
}

impl Render for Swoop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title(&format!("{}{}", self.tabs[self.active].name, self.t().title_suffix));
        let active_terminal = self.tabs[self.active].view.clone();
        let battery = *self.battery_percent.lock().unwrap();
        let tab_bar = self.render_tab_bar(battery, window, cx);
        let context_menu = if self.renaming.is_none()
            && self.exit_confirm.is_none()
            && self.close_confirm.is_none()
            && !self.show_qr
            && !self.show_preferences
        {
            self.render_context_menu(cx)
        } else {
            None
        };
        let rename_input = self.render_rename_input(cx);
        let exit_confirm = self.render_exit_confirm(cx);
        let close_confirm = self.render_close_confirm(cx);
        if self.renaming.is_some() {
            self.rename_focus.focus(window);
        }

        let bg_color = if battery.is_some_and(|b| b < 10) {
            rgba(0x3a05_05b8)
        } else if battery.is_some_and(|b| b < 20) {
            rgba(0x2d08_08b8)
        } else {
            rgba(0x1414_14b8)
        };

        let mut root = div()
            .id("app-root")
            .size_full()
            .bg(bg_color)
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                if ks.modifiers.alt && ks.key.as_str() == "tab" {
                    this.tabs[this.active].deactivate();
                    this.active = (this.active + 1) % this.tabs.len();
                    this.tabs[this.active].activate();
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

        if !self.toasts.is_empty() {
            let toast_bg: Hsla = rgb(0x2d_2d2d).into();
            let toast_fg: Hsla = rgb(0xcc_cccc).into();
            let toast_border: Hsla = rgb(0x00_7acc).into();
            let link_fg: Hsla = rgb(0x37_94ff).into();
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

fn main() {
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
        let window_handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_background: WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |window, cx| {
                    window.toggle_fullscreen();
                    cx.new(|cx| Swoop::new(window, cx))
                },
            )
            .unwrap();

        spawn_hotkey_listener(window_handle, cx);
    });
}

fn spawn_hotkey_listener(window_handle: WindowHandle<Swoop>, cx: &mut App) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();

    platform::grab_hotkeys(move || {
        let _ = tx.send(());
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
