// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `render_hotkey_picker` — the global-hotkey capture overlay. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_hotkey_picker(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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
                        // Swallow the dismiss click so it doesn't also land on
                        // whatever control sits under the overlay (a theme row,
                        // a hotkey "×", Save/Cancel).
                        cx.stop_propagation();
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
                        // stop_propagation (not a no-op) so a click inside the
                        // box doesn't reach the overlay's dismiss handler behind
                        // it and close the picker.
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                        })
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
