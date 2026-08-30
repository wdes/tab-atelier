// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! the quit / close-tab confirm dialogs. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_exit_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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

    pub(in crate::app) fn render_close_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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
}
