// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod terminal;
mod terminal_utils;

use gpui::*;
use std::path::PathBuf;
use swoop::{FontConfig, SavedState, TabState, load_font_config, load_state, save_state};
use terminal::TerminalView;

struct Tab {
    view: Entity<TerminalView>,
    name: String,
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
    exit_confirm: Option<ExitConfirm>,
    font_config: FontConfig,
}

impl Swoop {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename_focus = cx.focus_handle();
        let font_config = load_font_config();

        let (tabs, active) = if let Some(saved) = load_state() {
            let mut tabs = Vec::new();
            for ts in &saved.tabs {
                let cwd = ts.cwd.as_ref().map(|p| PathBuf::from(p));
                let fc = font_config.clone();
                let view = cx.new(|cx| TerminalView::new(cwd.as_deref(), fc, window, cx));
                tabs.push(Tab { view, name: ts.name.clone() });
            }
            if tabs.is_empty() {
                let fc = font_config.clone();
                let view = cx.new(|cx| TerminalView::new(None, fc, window, cx));
                tabs.push(Tab { view, name: "Terminal".into() });
            }
            let active = saved.active.min(tabs.len() - 1);
            (tabs, active)
        } else {
            let fc = font_config.clone();
            let view = cx.new(|cx| TerminalView::new(None, fc, window, cx));
            (vec![Tab { view, name: "Terminal".into() }], 0)
        };

        cx.spawn(async |this: WeakEntity<Swoop>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let Ok(()) = this.update(cx, |swoop, cx| {
                    swoop.persist(cx);
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
                let Ok(()) = this.update(cx, |swoop, cx| {
                    if swoop.exit_confirm.is_some() {
                        return;
                    }
                    for (i, tab) in swoop.tabs.iter().enumerate() {
                        if tab.view.read(cx).has_exited() {
                            swoop.exit_confirm = Some(ExitConfirm { tab_idx: i });
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

        Self {
            tabs,
            active,
            context_menu: None,
            renaming: None,
            rename_focus,
            visible: true,
            exit_confirm: None,
            font_config,
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
        self.tabs.push(Tab { view, name: format!("Terminal {}", idx + 1) });
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

    fn persist(&self, cx: &Context<Self>) {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let pid = tab.view.read(cx).pid();
                let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                TabState { name: tab.name.clone(), cwd }
            })
            .collect();
        save_state(&SavedState { tabs, active: self.active });
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
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        self.persist(cx);
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
                .on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                    this.active = i;
                    this.context_menu = None;
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
                .child(name);

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
        let item_count = match menu.kind {
            MenuKind::Tab(_) => if self.tabs.len() > 1 { 6 } else { 5 },
            MenuKind::Background => 4,
        };
        let menu_height = px(item_count as f32 * 27.0 + 8.0);

        let mut container = div()
            .id("context-menu")
            .absolute()
            .left(pos.x);

        container = if menu.open_upward {
            container.top(pos.y - menu_height)
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
                            this.close_tab(idx, cx);
                        }))
                        .child("Close"),
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
                        if let Some(item) = cx.read_from_clipboard() {
                            if let Some(text) = item.text() {
                                let view = &this.tabs[this.active].view;
                                view.read(cx).send_clipboard(text.to_string());
                            }
                        }
                        this.context_menu = None;
                        cx.notify();
                    }))
                    .child("Paste"),
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
                                    if let Some((i, ref text)) = this.renaming {
                                        if i < this.tabs.len() {
                                            this.tabs[i].name = text.clone();
                                        }
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
    }

impl Render for Swoop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_terminal = self.tabs[self.active].view.clone();
        let tab_bar = self.render_tab_bar(window, cx);
        let context_menu = if self.renaming.is_none() && self.exit_confirm.is_none() {
            self.render_context_menu(cx)
        } else {
            None
        };
        let rename_input = self.render_rename_input(cx);
        let exit_confirm = self.render_exit_confirm(cx);
        if self.renaming.is_some() {
            self.rename_focus.focus(window);
        }

        let mut root = div()
            .id("swoop-root")
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

        root
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
    let f12_keycode = 96u8;

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
            f12_keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
    }
    let _ = conn.flush();

    let (tx, rx) = std::sync::mpsc::channel::<()>();

    std::thread::spawn(move || {
        loop {
            match conn.wait_for_event() {
                Ok(event) => {
                    if let x11rb::protocol::Event::KeyPress(_) = event {
                        let _ = tx.send(());
                    }
                }
                Err(_) => break,
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
                    let _ = window_handle.update(cx, |swoop, window, _cx| {
                        swoop.visible = !swoop.visible;
                        if swoop.visible {
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
