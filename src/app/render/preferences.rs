// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `render_preferences` — the Preferences panel. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_preferences(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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

        let mut cursor_options = div().flex().flex_col().gap(px(4.0)).mt(px(8.0));
        for &st in CursorStyle::ALL {
            let is_active = st == self.cursor_style;
            cursor_options = cursor_options.child(
                div()
                    .id(SharedString::from(format!("pref-cursor-{}", st.id())))
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .bg(if is_active { option_active } else { option_bg })
                    .hover(|s| s.bg(if is_active { option_active } else { btn_hover }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.cursor_style = st;
                            for tab in &this.tabs {
                                tab.view.update(cx, |tv, _cx| tv.set_cursor_style(st));
                            }
                            cx.notify();
                        }),
                    )
                    .child(st.label()),
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

        // Global "max RAM per tab" default. A byte count with an optional
        // K/M/G/T suffix (e.g. `8G`); empty = unlimited. Applies to tabs spawned
        // afterwards and is the RAM gauge's 100% mark. Char filter: digits +
        // suffix letters only.
        let default_mem_text = self.pref_default_mem_text.clone();
        let default_mem_input = div()
            .id("pref-default-mem-input")
            .key_context("pref-default-mem")
            .track_focus(&self.pref_default_mem_focus)
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
                    this.pref_default_mem_focus.focus(window);
                    cx.notify();
                }),
            )
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, _window, cx| match ev.keystroke.key.as_str() {
                    "backspace" => {
                        this.pref_default_mem_text.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ref ch) = ev.keystroke.key_char
                            && ch.chars().all(is_mem_char)
                            && this.pref_default_mem_text.len() + ch.len() <= 12
                        {
                            this.pref_default_mem_text.push_str(ch);
                            cx.notify();
                        }
                    }
                }),
            )
            .when(default_mem_text.is_empty(), |el| {
                el.child(div().text_color(placeholder_fg).child("unlimited (e.g. 8G)"))
            })
            .when(!default_mem_text.is_empty(), |el| {
                el.child(default_mem_text)
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
                // Swallow ALL mouse input so a click on the dimmed area can't
                // fall through to the tab strip's on_click underneath (which
                // was activating whatever tab sat below the cursor when the
                // dialog was dismissed). The empty handlers below don't stop
                // propagation on their own; occlude does.
                .occlude()
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
                        // Two-column body: wide enough to fit both columns on a
                        // normal screen (halving the height so it fits short
                        // screens), capped to 95% width on narrow ones. The
                        // 90%-height cap + vertical scroll is the fallback when
                        // even two columns are taller than the viewport.
                        .w(px(860.0))
                        .max_w(relative(0.95))
                        .max_h(relative(0.9))
                        .overflow_y_scroll()
                        .text_size(px(14.0))
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .child(div().text_size(px(16.0)).mb(px(16.0)).child(t.preferences))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(24.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .gap(px(16.0))
                                        .child(div().child(t.theme).child(theme_options))
                                        .child(div().child("Cursor").child(cursor_options))
                                        .child(div().child(t.opacity).child(opacity_slider))
                                        .child(div().child(t.toggle_hotkeys).child(hotkey_list))
                                        .child(div().child(t.language).child(lang_options))
                                        .child(div().child(t.browser).child(browser_input)),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .gap(px(16.0))
                                        .child(div().child(t.code_editor).child(editor_input))
                                        .child(div().child(t.api_addr).child(api_addr_input))
                                        .child(div().child(t.api_tls_addr).child(api_tls_addr_input))
                                        .child(div().child(t.share_url_base).child(share_url_base_input))
                                        .child(div().child(t.default_tab_ram).child(default_mem_input)),
                                ),
                        )
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
                                                // Global "max RAM per tab" default. Empty = unlimited;
                                                // otherwise must parse as a byte count (e.g. "8G") to be
                                                // accepted — an unparseable entry keeps the prior value.
                                                let mem_in = this.pref_default_mem_text.trim().to_string();
                                                let mem_valid = !mem_in.is_empty()
                                                    && crate::TabResourceLimits {
                                                        memory_max: Some(mem_in.clone()),
                                                        ..Default::default()
                                                    }
                                                    .memory_max_bytes()
                                                    .is_some();
                                                if mem_in.is_empty() {
                                                    this.default_tab_mem_max = None;
                                                } else if mem_valid {
                                                    this.default_tab_mem_max = Some(mem_in);
                                                }
                                                // Reflect it immediately for the gauge + new-tab spawns.
                                                #[cfg(target_os = "linux")]
                                                {
                                                    this.default_limits.memory_max = this.default_tab_mem_max.clone();
                                                }
                                                let default_mem_max = this.default_tab_mem_max.clone();
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
                                                        cursor_style: Some(this.cursor_style.id().into()),
                                                        opacity: Some(this.opacity),
                                                        // Menu-toggled, not in this dialog — carry the
                                                        // on-disk value through so saving prefs doesn't
                                                        // wipe the gauge setting.
                                                        show_tab_gauge: this.show_tab_gauge,
                                                        // Menu-toggled (right-click), not in this dialog —
                                                        // carry it through so saving prefs doesn't drop it.
                                                        claude_only: this.claude_only,
                                                        // Relay + env are set via CLI/menu with
                                                        // their own persist paths — carry the
                                                        // on-disk values so the dialog save
                                                        // doesn't clobber them.
                                                        relay_mode: this.relay_mode,
                                                        relay_endpoint_id: on_disk_prefs.relay_endpoint_id,
                                                        relay_egress: on_disk_prefs.relay_egress,
                                                        tab_env: on_disk_prefs.tab_env,
                                                        // Dashboard repo→service map — CLI/config managed,
                                                        // not in this dialog; carry the on-disk value through.
                                                        repo_families: on_disk_prefs.repo_families,
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
                                                        // The GUI dialog edits only the RAM cap
                                                        // (memory_max); the CPU/tasks axes are headless-
                                                        // only, so carry them through from disk rather
                                                        // than resetting the whole struct to Default
                                                        // (which silently wiped a CLI-set cpu/tasks cap).
                                                        default_tab_limits: crate::TabResourceLimits {
                                                            memory_max: default_mem_max,
                                                            cpu_quota_percent: on_disk_prefs
                                                                .default_tab_limits
                                                                .cpu_quota_percent,
                                                            tasks_max: on_disk_prefs.default_tab_limits.tasks_max,
                                                        },
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

}
