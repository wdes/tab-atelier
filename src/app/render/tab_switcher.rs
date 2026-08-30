// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! the Ctrl-Tab MRU switcher overlay. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    /// Open the Ctrl+P MRU tab switcher: list every tab EXCEPT the current one,
    /// most-recently-visited first, highlighting the previous tab so a bare
    /// Ctrl+P → Enter jumps straight back. No-op with fewer than two tabs.
    pub(in crate::app) fn open_tab_switcher(&mut self, cx: &mut Context<Self>) {
        // Need something to switch to, and don't stack over another modal.
        if self.tabs.len() < 2
            || self.show_preferences
            || self.show_hotkey_picker
            || self.show_qr
            || self.renaming.is_some()
            || self.exit_confirm.is_some()
            || self.close_confirm.is_some()
        {
            return;
        }
        // Order by `last_used_at` (the same field the mobile remote sorts by)
        // so desktop Ctrl+P and the phone agree — and so a tab opened on the
        // phone (viewer attach bumps last_used_at) floats up here too.
        let keys: Vec<Option<u64>> = self.tabs.iter().map(|t| t.last_used_at).collect();
        let order = mru_tab_order(self.active, &keys);
        self.tab_switcher = Some(TabSwitcher { order, selected: 0 });
        cx.notify();
    }

    /// Close the switcher without switching, returning focus to the terminal.
    pub(in crate::app) fn close_tab_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_switcher = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(in crate::app) fn render_tab_switcher(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let sw = self.tab_switcher.as_ref()?;
        let th = self.th();
        let dialog_bg = th.surface_hsla();
        let dialog_fg = th.fg_hsla();
        let dialog_border = th.border_hsla();
        let sel_bg = th.accent_hsla();
        let hover_bg = th.selection_hsla();
        let muted = th.border_hsla();

        let mut list = div().flex().flex_col().gap(px(2.0)).mt(px(10.0));
        for (row, &idx) in sw.order.iter().enumerate() {
            if idx >= self.tabs.len() {
                continue;
            }
            let tab = &self.tabs[idx];
            let name = tab.name.clone();
            // "… ago" from the same last_used_at that drives the order, so the
            // label can't disagree with the row's position.
            let ago = tab.last_used_at.map_or_else(
                || "never".to_string(),
                |ms| {
                    let elapsed = std::time::Duration::from_millis(crate::unix_millis().saturating_sub(ms));
                    format!("{} ago", format_duration(elapsed))
                },
            );
            let selected = row == sw.selected;
            list = list.child(
                div()
                    .id(SharedString::from(format!("switcher-row-{row}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(16.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(sel_bg))
                    .when(!selected, |d| d.hover(|s| s.bg(hover_bg)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                            this.tab_switcher = None;
                            this.select_tab(idx, window, cx);
                        }),
                    )
                    .child(div().overflow_hidden().child(name.to_string()))
                    .child(div().flex_none().text_size(px(12.0)).text_color(muted).child(ago)),
            );
        }

        Some(
            div()
                .id("tab-switcher-overlay")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .track_focus(&self.tab_switcher_focus)
                .bg(Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                        this.close_tab_switcher(window, cx);
                    }),
                )
                .child(
                    div()
                        // Clicks inside the card must not hit the overlay's
                        // close handler.
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .bg(dialog_bg)
                        .border_1()
                        .border_color(dialog_border)
                        .rounded(px(6.0))
                        .p(px(16.0))
                        .min_w(px(360.0))
                        .max_w(px(560.0))
                        .max_h(px(440.0))
                        .overflow_hidden()
                        .text_color(dialog_fg)
                        .text_size(px(14.0))
                        .child(div().text_size(px(13.0)).text_color(muted).child("Recent tabs"))
                        .child(list)
                        .child(
                            div().mt(px(10.0)).text_size(px(11.0)).text_color(muted).child(
                                "\u{2191}\u{2193} select \u{b7} Ctrl+P cycle \u{b7} Enter open \u{b7} Esc cancel",
                            ),
                        ),
                ),
        )
    }
}
