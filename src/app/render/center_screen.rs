// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `impl Render for AppState` — the top-level window layout. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drain any tabs the API thread asked us to create. We can't call
        // insert_tab from persist() because that path doesn't have a
        // Window handle; piggy-backing on render() is the simplest place
        // to react to remote POST /tabs requests.
        //
        // Only touch the global API mutex when the lock-free activity
        // counter moved since the last frame — POST /tabs, like every
        // authenticated request, bumps it, so a quiet counter proves
        // `pending_new_tabs` is 0. The unconditional lock made every
        // frame contend with whatever an API handler was doing under
        // the same mutex (e.g. the /tabs body rebuild).
        let seq = self.activity_signal.load(std::sync::atomic::Ordering::Relaxed);
        let (new_tab_count, new_tab_cwds): (usize, Vec<PathBuf>) = if seq == self.render_activity_seen.get() {
            (0, Vec::new())
        } else {
            self.render_activity_seen.set(seq);
            let mut snap = self.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        // No tab to show yet (transient empty state / future async boot): the
        // reusable centered screen stands in rather than indexing a missing tab.
        if self.tabs.is_empty() {
            return self.render_center_screen("Tab Atelier", self.t().loading, None);
        }
        // The active tab must be live to display it — fork its shell now if it
        // was still a skeleton (e.g. the user switched to a not-yet-warmed tab
        // before the boot loader reached it). No-op once spawned.
        self.tabs[self.active].view.update(cx, |v, _| v.ensure_spawned());
        // Only push the title when it changed — gpui does no diffing, so an
        // unconditional call here meant a format! + X11 property write on
        // every frame (30-60 fps while the terminal streams) for a string
        // that only moves on tab switch/rename.
        let title = format!("{}{}", self.tabs[self.active].name, self.t().title_suffix);
        if self.last_window_title != title {
            window.set_window_title(&title);
            self.last_window_title = title;
        }
        let active_terminal = self.tabs[self.active].view.clone();
        #[cfg(feature = "energy")]
        let battery = *self
            .battery_percent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(not(feature = "energy"))]
        let battery: Option<u8> = None;
        // Per-tab ledges for the pet are collected inside `render_tab_bar` by
        // measuring canvases (see `pet_ledges`).
        let tab_bar = self.render_tab_bar(battery, window, cx);
        let context_menu = if self.renaming.is_none()
            && self.exit_confirm.is_none()
            && self.close_confirm.is_none()
            && !self.show_qr
            && !self.show_preferences
            && self.tab_switcher.is_none()
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
        // Anchor focus on the switcher while it's open so its keys (↑↓/Enter/
        // Esc) hit this modal instead of leaking into the terminal behind it.
        if self.tab_switcher.is_some() {
            self.tab_switcher_focus.focus(window);
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
        // Battery-low red wash. It used to live on `#app-root`'s bg
        // (above), but commit 24ac421 ("terminal: paint an opaque base
        // bg so default cells overwrite the last frame") made the
        // terminal fill its whole area with an opaque `term_bg` quad,
        // which occludes the root bg — so on low battery only the tab
        // bar's blink survived. Float the tint as a translucent overlay
        // ABOVE the terminal instead. Steady (not blinking); the tab bar
        // keeps its own blink. 0xRRGGBBAA, matching `rgba` above.
        let battery_tint = if battery.is_some_and(|b| b < 10) {
            Some(rgba(0xff00_002e)) // ~18% red
        } else if battery.is_some_and(|b| b < 20) {
            Some(rgba(0xff00_0019)) // ~10% red
        } else {
            None
        };

        let mut root = div()
            .id("app-root")
            .size_full()
            .bg(bg_color)
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                // Ctrl+P MRU tab switcher. While it's open the modal holds
                // focus (so the terminal gets no keys); these arrive here by
                // bubbling. Handle every switcher key and return so nothing
                // below (alt+tab / ctrl+shift+t) also fires.
                if this.tab_switcher.is_some() {
                    // Bounds / selection track the FILTERED list, not the full
                    // captured order, so ↑↓/Enter/cycle operate on what's shown.
                    let filtered = this.switcher_filtered();
                    let len = filtered.len();
                    match ks.key.as_str() {
                        "escape" => this.close_tab_switcher(window, cx),
                        "up" => {
                            if let Some(s) = this.tab_switcher.as_mut() {
                                s.selected = s.selected.saturating_sub(1);
                            }
                            cx.notify();
                        }
                        "down" => {
                            if let Some(s) = this.tab_switcher.as_mut()
                                && s.selected + 1 < len
                            {
                                s.selected += 1;
                            }
                            cx.notify();
                        }
                        // Tapping Ctrl+P again cycles the highlight downward.
                        "p" if ks.modifiers.control && len > 0 => {
                            if let Some(s) = this.tab_switcher.as_mut() {
                                s.selected = (s.selected + 1) % len;
                            }
                            cx.notify();
                        }
                        "enter" => {
                            let pick = filtered
                                .get(this.tab_switcher.as_ref().map_or(0, |s| s.selected))
                                .copied();
                            this.tab_switcher = None;
                            match pick {
                                Some(idx) => this.select_tab(idx, window, cx),
                                None => this.close_tab_switcher(window, cx),
                            }
                        }
                        "backspace" => {
                            if let Some(s) = this.tab_switcher.as_mut() {
                                s.query.pop();
                                s.selected = 0;
                            }
                            cx.notify();
                        }
                        // Any printable character narrows the filter. `key_char`
                        // is None for the arrows/enter/ctrl-chords above, so a
                        // bare Ctrl+P cycles rather than typing 'p'.
                        _ => {
                            if let Some(ch) = ks.key_char.as_ref().filter(|c| !c.is_empty() && !ks.modifiers.control)
                                && let Some(s) = this.tab_switcher.as_mut()
                            {
                                s.query.push_str(ch);
                                s.selected = 0;
                                cx.notify();
                            }
                        }
                    }
                    return;
                }
                if ks.modifiers.control && !ks.modifiers.shift && !ks.modifiers.alt && ks.key.as_str() == "p" {
                    this.open_tab_switcher(cx);
                    return;
                }
                if ks.modifiers.control && ks.modifiers.shift && ks.key.as_str() == "t" {
                    this.add_tab_after_current(window, cx);
                    return;
                }
                if ks.modifiers.alt && ks.key.as_str() == "tab" {
                    let next = (this.active + 1) % this.tabs.len();
                    this.select_tab(next, window, cx);
                }
            }))
            .child(
                div()
                    .id("terminal-area")
                    .relative()
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
                            // Grab the link under the cursor (if the right-click
                            // landed on a detected URL/path) so the menu can offer
                            // "Copy path (link)". The hover cell tracks the mouse,
                            // so it already points at the clicked cell.
                            let link = this.tabs[this.active].view.read(cx).hovered_url();
                            this.context_menu = Some(ContextMenu {
                                kind: MenuKind::Background,
                                position: ev.position,
                                open_upward: false,
                                link,
                            });
                            cx.notify();
                        }),
                    )
                    .child(active_terminal)
                    // Low-battery red wash, anchored to `terminal-area`
                    // (hence the `.relative()` above) so it covers the
                    // terminal but leaves the tab bar's blink untouched.
                    // Non-interactive → mouse events pass through to the
                    // terminal below.
                    .when_some(battery_tint, |area, tint| {
                        area.child(div().absolute().top(px(0.0)).left(px(0.0)).size_full().bg(tint))
                    }),
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

        if let Some(switcher) = self.render_tab_switcher(cx) {
            root = root.child(switcher);
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

        // Advance + draw the screen-mate pet on top of everything (all the logic
        // lives in PetOverlay). The ~50 ms notify loop drives the frames; it's
        // frozen while the window is hidden.
        #[cfg(feature = "pets")]
        {
            let vp = window.viewport_size();
            let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
            let visible = self.visible;
            if let Some(el) = self.pet.render(visible, vw, vh, cx, |this| &mut this.pet) {
                root = root.child(el);
            }
        }

        root.into_any_element()
    }
}
