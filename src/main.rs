// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod power;
mod terminal;
mod terminal_utils;
mod tracking;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use gpui::*;
use gpui::prelude::FluentBuilder;
use tab_atelier::{FontConfig, SavedState, TabState, load_font_config, load_state, load_wakatime_key, save_state};
use terminal::TerminalView;
use tracking::WakatimeTracker;

struct Tab {
    view: Entity<TerminalView>,
    name: String,
    created_at: std::time::Instant,
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

struct ExitConfirm {
    tab_idx: usize,
}

struct Swoop {
    tabs: Vec<Tab>,
    active: usize,
    context_menu: Option<ContextMenu>,
    renaming: Option<(usize, String)>,
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
}

impl Swoop {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename_focus = cx.focus_handle();
        let font_config = load_font_config();

        let (tabs, active) = if let Some(saved) = load_state() {
            let mut tabs = Vec::new();
            for ts in &saved.tabs {
                let cwd = ts.cwd.as_ref().map(PathBuf::from);
                let fc = font_config.clone();
                let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, window, cx));
                if let Some(ref output) = ts.output {
                    view.read(cx).restore_output(output);
                }
                tabs.push(Tab { view, name: ts.name.clone(), created_at: std::time::Instant::now() });
            }
            if tabs.is_empty() {
                let fc = font_config.clone();
                let view = cx.new(|cx| TerminalView::new(None, fc, window, cx));
                tabs.push(Tab { view, name: "Terminal".into(), created_at: std::time::Instant::now() });
            }
            let active = saved.active.min(tabs.len() - 1);
            (tabs, active)
        } else {
            let fc = font_config.clone();
            let view = cx.new(|cx| TerminalView::new(None, fc, window, cx));
            (vec![Tab { view, name: "Terminal".into(), created_at: std::time::Instant::now() }], 0)
        };

        cx.spawn(async |this: WeakEntity<Swoop>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.persist(cx);
                }) else {
                    break;
                };
            }
        })
        .detach();

        cx.spawn(async |this: WeakEntity<Swoop>, cx: &mut AsyncApp| {
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

        tabs[active].view.read(cx).focus_handle(cx).focus(window);

        let tracker = load_wakatime_key().map(WakatimeTracker::new);

        let api_token = api::generate_token();
        let api_state = Arc::new(Mutex::new(api::TabSnapshot {
            tabs: Vec::new(),
            active: 0,
            power: Vec::new(),
            pending_closes: Vec::new(),
        }));
        api::start_api_server(api_state.clone(), api_token.clone());

        let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let power_watts: Arc<Mutex<Vec<power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
        power::start_power_monitor(power_pids.clone(), power_watts.clone());

        Self {
            tabs,
            active,
            context_menu: None,
            renaming: None,
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
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = {
            let pid = self.tabs[self.active].view.read(cx).pid();
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        };
        let fc = self.font_config.clone();
        let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, window, cx));
        let idx = self.tabs.len();
        self.tabs.push(Tab { view, name: format!("Terminal {}", idx + 1), created_at: std::time::Instant::now() });
        self.active = idx;
        cx.notify();
    }

    fn close_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs[idx].view.read(cx).shutdown();
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > idx {
            self.active -= 1;
        }
        self.context_menu = None;
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let pid = tab.view.read(cx).pid();
                let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                TabState { name: tab.name.clone(), cwd, output: None }
            })
            .collect();
        let api_tabs: Vec<(String, Option<String>)> = tabs
            .iter()
            .map(|t| (t.name.clone(), t.cwd.clone()))
            .collect();

        save_state(&SavedState { tabs, active: self.active });

        {
            let mut snapshot = self.api_state.lock().unwrap();
            snapshot.tabs = api_tabs;
            snapshot.active = self.active;
            snapshot.power = self.power_watts.lock().unwrap().clone();
        }

        {
            let pids: Vec<u32> = self
                .tabs
                .iter()
                .map(|tab| tab.view.read(cx).pid())
                .collect();
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
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
            tracker.record_activity(cwd);
        }
    }

    fn respawn_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = std::fs::read_link(format!("/proc/{old_pid}/cwd"))
            .ok()
            .or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.read(cx).shutdown();
        let fc = self.font_config.clone();
        let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, window, cx));
        self.tabs[idx].view = view;
        self.tabs[idx].created_at = std::time::Instant::now();
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn respawn_tab_with_history(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = std::fs::read_link(format!("/proc/{old_pid}/cwd"))
            .ok()
            .or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.update(cx, |view, cx| {
            view.respawn(cwd.as_deref(), cx);
        });
        self.tabs[idx].created_at = std::time::Instant::now();
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
                let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                let output = {
                    let text = tab.view.read(cx).copy_all_history();
                    if text.is_empty() { None } else { Some(text) }
                };
                TabState { name: tab.name.clone(), cwd, output }
            })
            .collect();
        save_state(&SavedState { tabs, active: self.active });

        if let Some(ref tracker) = self.tracker {
            tracker.shutdown();
        }
        for tab in &self.tabs {
            tab.view.read(cx).shutdown();
        }
        cx.quit();
    }

    fn render_tab_bar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let tab_bg: Hsla = rgb(0x1e1e1e).into();
        let tab_active_bg: Hsla = rgb(0x2d2d2d).into();
        let tab_fg: Hsla = rgb(0xcccccc).into();
        let tab_border: Hsla = rgb(0x3c3c3c).into();
        let watts_fg: Hsla = rgb(0x888888).into();

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

            let power_label = watts.get(i).map(|tp| tp.label()).unwrap_or_default();

            let tab_el = div()
                .id(ElementId::Name(format!("tab-{i}").into()))
                .flex()
                .items_center()
                .px(px(12.0))
                .h_full()
                .bg(if is_active { tab_active_bg } else { tab_bg })
                .border_r_1()
                .border_color(tab_border)
                .text_color(tab_fg)
                .text_size(px(13.0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _ev: &ClickEvent, window, cx| {
                    this.active = i;
                    this.context_menu = None;
                    this.tabs[i].view.read(cx).focus_handle(cx).focus(window);
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
                .child(name)
                .when(!power_label.is_empty(), |el: Stateful<Div>| {
                    el.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(watts_fg)
                            .min_w(px(55.0))
                            .text_align(gpui::TextAlign::Right)
                            .child(power_label),
                    )
                });

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
        let menu_bg: Hsla = rgb(0x252526).into();
        let menu_fg: Hsla = rgb(0xcccccc).into();
        let menu_hover: Hsla = rgb(0x094771).into();
        let menu_border: Hsla = rgb(0x3c3c3c).into();

        let pos = menu.position;

        let mut container = div()
            .id("context-menu")
            .absolute()
            .left(pos.x);

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

        if let MenuKind::Tab(idx) = menu.kind {
            container = container.child(
                div()
                    .id("menu-rename")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                        let name = this.tabs[idx].name.clone();
                        this.renaming = Some((idx, name));
                        this.context_menu = None;
                        this.rename_focus.focus(window);
                        cx.notify();
                    }))
                    .child("Rename"),
            );

            if self.tabs.len() > 1 {
                container = container.child(
                    div()
                        .id("menu-close")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.close_confirm = Some(idx);
                            this.context_menu = None;
                            cx.notify();
                        }))
                        .child("Close"),
                );
            }

            let stat_fg: Hsla = rgb(0x888888).into();
            let elapsed = self.tabs[idx].created_at.elapsed();
            let power = self.power_watts.lock().unwrap();
            let power_info = power.get(idx);

            let mut stats_lines: Vec<String> = Vec::new();

            if let Some(p) = power_info {
                stats_lines.push(format!("CPU: {}", p.cpu_label()));
                if let Some(w) = p.watts {
                    stats_lines.push(format!("Power: {}", p.label()));
                    let wh = w * elapsed.as_secs_f64() / 3600.0;
                    if wh >= 1.0 {
                        stats_lines.push(format!("Energy: {wh:.1} Wh"));
                    } else {
                        stats_lines.push(format!("Energy: {:.0} mWh", wh * 1000.0));
                    }
                }
            }
            stats_lines.push(format!("Uptime: {}", format_duration(elapsed)));

            container = container.child(
                div()
                    .mx(px(8.0))
                    .my(px(4.0))
                    .h(px(1.0))
                    .bg(menu_border),
            );
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

        container = container
            .child(
                div()
                    .id("menu-copy")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        if let Some(text) = this.tabs[this.active].view.read(cx).copy_selection() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Copy"),
            )
            .child(
                div()
                    .id("menu-copy-all")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        let text = this.tabs[this.active].view.read(cx).copy_all_history();
                        if !text.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Copy All"),
            )
            .child(
                div()
                    .id("menu-paste")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        if let Some(item) = cx.read_from_clipboard()
                            && let Some(text) = item.text()
                        {
                            let view = &this.tabs[this.active].view;
                            view.read(cx).send_clipboard(text.to_string());
                        }
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Paste"),
            )
            .child(
                div()
                    .id("menu-reset")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        this.tabs[this.active].view.read(cx).reset_terminal();
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Reset input & color"),
            )
            .child(
                div()
                    .id("menu-windowed")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                        this.windowed = !this.windowed;
                        window.toggle_fullscreen();
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child(if self.windowed { "Fullscreen mode" } else { "Windowed mode" }),
            )
            .child(
                div()
                    .id("menu-close-all")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        this.close_all_tabs(cx);
                    }))
                    .child("Close All"),
            )
            .child(
                div()
                    .id("menu-remote")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                        this.show_qr = true;
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Remote control"),
            );

        Some(container)
    }

    fn render_rename_input(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let (_idx, text) = self.renaming.as_ref()?;
        let text = text.clone();
        let input_bg: Hsla = rgb(0x1e1e1e).into();
        let input_fg: Hsla = rgb(0xcccccc).into();
        let input_border: Hsla = rgb(0x007acc).into();
        let cursor_color: Hsla = rgb(0xcccccc).into();

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
                .bg(Hsla::from(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.5 }))
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
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "escape" => {
                                    this.renaming = None;
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "backspace" => {
                                    if let Some((_, ref mut text)) = this.renaming {
                                        text.pop();
                                    }
                                    cx.notify();
                                }
                                _ => {
                                    if let Some(ref ch) = ev.keystroke.key_char {
                                        if let Some((_, ref mut text)) = this.renaming {
                                            text.push_str(ch);
                                        }
                                        cx.notify();
                                    }
                                }
                            }
                        }))
                        .child("Rename tab:")
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .mt(px(8.0))
                                .bg(rgb(0x141414))
                                .border_1()
                                .border_color(input_border)
                                .rounded(px(3.0))
                                .px(px(8.0))
                                .py(px(4.0))
                                .min_h(px(28.0))
                                .cursor_text()
                                .child(text)
                                .child(
                                    div()
                                        .w(px(1.0))
                                        .h(px(16.0))
                                        .bg(cursor_color),
                                ),
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

        let dialog_bg: Hsla = rgb(0x252526).into();
        let dialog_fg: Hsla = rgb(0xcccccc).into();
        let dialog_border: Hsla = rgb(0x3c3c3c).into();
        let btn_bg: Hsla = rgb(0x0e639c).into();
        let btn_hover: Hsla = rgb(0x1177bb).into();
        let btn_secondary_bg: Hsla = rgb(0x3c3c3c).into();
        let btn_secondary_hover: Hsla = rgb(0x505050).into();

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
                .bg(Hsla::from(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.5 }))
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
                                .child(format!("Shell exited in \"{}\"", tab_name)),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(13.0))
                                .text_color(rgb(0x999999))
                                .child("Close this tab or reopen a new shell?"),
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
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                            this.respawn_tab(idx, window, cx);
                                        }))
                                        .child("Reopen (clean)"),
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
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                            this.respawn_tab_with_history(idx, window, cx);
                                        }))
                                        .child("Reopen (with history)"),
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
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                            this.exit_confirm = None;
                                            if this.tabs.len() <= 1 {
                                                this.close_all_tabs(cx);
                                            } else {
                                                this.close_tab(idx, cx);
                                            }
                                        }))
                                        .child("Close Tab"),
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

        let dialog_bg: Hsla = rgb(0x252526).into();
        let dialog_fg: Hsla = rgb(0xcccccc).into();
        let dialog_border: Hsla = rgb(0x3c3c3c).into();
        let btn_bg: Hsla = rgb(0x0e639c).into();
        let btn_hover: Hsla = rgb(0x1177bb).into();
        let btn_secondary_bg: Hsla = rgb(0x3c3c3c).into();
        let btn_secondary_hover: Hsla = rgb(0x505050).into();

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
                .bg(Hsla::from(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.5 }))
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
                        .child(
                            div()
                                .text_size(px(15.0))
                                .child(format!("Close \"{}\"?", tab_name)),
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
                                        .id("close-cancel")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_secondary_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_secondary_hover))
                                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                            this.close_confirm = None;
                                            cx.notify();
                                        }))
                                        .child("Cancel"),
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
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                            this.close_confirm = None;
                                            this.close_tab(idx, cx);
                                        }))
                                        .child("Close"),
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

        let qr = match qrcode::QrCode::new(url.as_bytes()) {
            Ok(q) => q,
            Err(_) => return None,
        };
        let matrix = qr.render::<char>()
            .quiet_zone(true)
            .module_dimensions(2, 1)
            .build();

        let dialog_bg: Hsla = rgb(0x252526).into();
        let dialog_fg: Hsla = rgb(0xcccccc).into();
        let dialog_border: Hsla = rgb(0x3c3c3c).into();
        let btn_bg: Hsla = rgb(0x0e639c).into();
        let btn_hover: Hsla = rgb(0x1177bb).into();

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
                .bg(Hsla::from(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.5 }))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                    this.show_qr = false;
                    cx.notify();
                }))
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
                        .child(
                            div()
                                .text_size(px(15.0))
                                .child("Scan to connect from your phone"),
                        )
                        .child(
                            div()
                                .mt(px(12.0))
                                .bg(gpui::white())
                                .rounded(px(4.0))
                                .p(px(8.0))
                                .child(
                                    div()
                                        .text_color(gpui::black())
                                        .text_size(px(6.0))
                                        .font_family("monospace")
                                        .child(matrix),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(11.0))
                                .text_color(rgb(0x999999))
                                .child(url),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .mt(px(12.0))
                                .child(
                                    div()
                                        .id("qr-close")
                                        .px(px(14.0))
                                        .py(px(6.0))
                                        .bg(btn_bg)
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(btn_hover))
                                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                            this.show_qr = false;
                                            cx.notify();
                                        }))
                                        .child("Close"),
                                ),
                        ),
                ),
        )
    }
}

impl Render for Swoop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title(&format!("{} — Tab Atelier", self.tabs[self.active].name));
        let active_terminal = self.tabs[self.active].view.clone();
        let tab_bar = self.render_tab_bar(window, cx);
        let context_menu = if self.renaming.is_none() && self.exit_confirm.is_none() && self.close_confirm.is_none() && !self.show_qr {
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

        let mut root = div()
            .id("app-root")
            .size_full()
            .bg(rgba(0x141414b8))
            .flex()
            .flex_col()
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
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            cx.notify();
                        })),
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

fn main() {
    Application::new().run(|cx: &mut App| {
        let window_handle = cx.open_window(
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
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};

    let (conn, _screen_num) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(_) => return,
    };

    let screen = &conn.setup().roots[_screen_num];
    let root = screen.root;
    let hotkeys: &[u8] = &[148, 49]; // XF86Calculator, œ

    for &keycode in hotkeys {
        for mask in [
            ModMask::default(),
            ModMask::LOCK,
            ModMask::from(u16::from(ModMask::M2)),
            ModMask::LOCK | ModMask::from(u16::from(ModMask::M2)),
        ] {
            let _ = conn.grab_key(
                false,
                root,
                mask,
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
        }
    }
    let _ = conn.flush();

    let (tx, rx) = std::sync::mpsc::channel::<()>();

    std::thread::spawn(move || {
        while let Ok(event) = conn.wait_for_event() {
            if let x11rb::protocol::Event::KeyPress(_) = event {
                let _ = tx.send(());
            }
        }
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
                            window.activate_window();
                        } else {
                            window.minimize_window();
                        }
                    });
                });
            }
        }
    })
    .detach();
}
