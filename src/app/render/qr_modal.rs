// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `render_qr_modal` — the share-QR overlay. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_qr_modal(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if !self.show_qr {
            return None;
        }
        let data = self.qr_modal.as_ref()?;
        let ips = &data.ips;
        let url = data.url.clone();
        let url_for_click = url.clone();

        let th = self.th();
        let dialog_bg = th.surface_hsla();
        let dialog_fg = th.fg_hsla();
        let dialog_border = th.border_hsla();
        let btn_bg = th.accent_hsla();
        let btn_hover = th.accent_hover_hsla();
        let link_fg = th.accent_hsla();

        let w = data.qr_width;
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
                let is_dark = data.qr_dark[row * w + col];
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

}
