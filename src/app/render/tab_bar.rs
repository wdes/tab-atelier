// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `render_tab_bar` — the top tab strip. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_tab_bar(
        &mut self,
        battery: Option<u8>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
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

        // Element ids are index-keyed and stable — build each "tab-{i}"
        // SharedString once and reuse it, instead of a format! per tab
        // per frame (the bar re-renders at 30-60 fps while the terminal
        // streams).
        while self.tab_el_ids.len() < self.tabs.len() {
            self.tab_el_ids
                .push(SharedString::from(format!("tab-{}", self.tab_el_ids.len())));
        }

        // Hold the guard for the (microseconds-long) bar build instead
        // of cloning the whole per-tab power Vec every frame.
        #[cfg(feature = "energy")]
        let watts = self
            .power_watts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
            .border_b_1()
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
                        // Tab-bar background — never over terminal text.
                        link: None,
                    });
                    cx.notify();
                }),
            );

        let theme_name = self.theme_name;
        // Per-tab RAM mini gauge (#28 S5): each bar is this tab's RSS as a
        // fraction of a real ceiling — the tab's effective memory cap (own over
        // the global default) when one is set, else total system RAM. So 100%
        // means "at the cap" (or "using all the machine's RAM"), not "heaviest
        // tab". Computed from the RSS the persist loop cached (no /proc walk
        // here). Off unless the pref is toggled on.
        let show_tab_gauge = self.show_tab_gauge;
        let sys_ram = if show_tab_gauge {
            crate::system_total_ram_bytes()
        } else {
            None
        };
        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            // Visual lock marker — a 🔒 ahead of the name is enough to
            // make "this tab won't accept input" obvious at a glance,
            // and stays out of the rename text (still raw name in the
            // rename editor).
            // Always show the tab's STABLE name in the strip — never the
            // in-progress rename text. The rename editor is the centered modal
            // (`render_rename_input`); mirroring the live text here grew this
            // tab as you typed, and the tab bar is `flex_wrap`, so a longer name
            // could wrap the strip onto a second row, shrink the terminal area,
            // and SIGWINCH-resize every background tab — whose redraw bumped
            // `last_output_at` and falsely lit their green "streaming" LED
            // (all tabs flashing green while renaming). A stable width avoids it.
            let base_name = tab.name.to_string();
            let name = if tab.locked && self.renaming.as_ref().is_none_or(|(ri, _)| *ri != i) {
                format!("🔒 {base_name}")
            } else {
                base_name
            };
            // Agent-state LED to the left of the tab name. Visible whenever a
            // session is attached (agent_kind set) OR a transient state is live;
            // cleared only when the session actually ends (the `idle` POST wipes
            // agent_kind too). Colour is an "unreviewed work" model:
            //   green  — the agent is working right now (thinking / streaming);
            //   blue   — it worked and has stopped, and you haven't reviewed
            //            this tab since (sticky until you focus it — set by the
            //            sweep above); "you have output to look at here";
            //   red    — the agent hit an error;
            //   grey   — nothing to review (never worked, or already reviewed).
            let session_attached = tab.agent_kind.is_some();
            // Is the agent PROCESS running? The catbus sweep stamps `agent_pid`
            // = Some for a live `claude`/`catbus-agent` descendant, None when
            // Gone. Without the sweep (catbus off) we can't tell, so assume
            // alive. `full_sweep_ran` gates the dim-red "dead" dot on the first
            // sweep having completed, so a restored agent doesn't flash red for
            // the first second or two after boot. The ⛑ brain watchdog is exempt
            // from "dead" (no session to resume); compute_tab_led skips it.
            #[cfg(feature = "catbus")]
            let (agent_alive, full_sweep_ran) = (
                tab.agent_pid.get().is_some(),
                self.last_agent_full_sweep.get().is_some(),
            );
            #[cfg(not(feature = "catbus"))]
            let (agent_alive, full_sweep_ran) = (true, false);
            // Single shared derivation so the desktop dot, the /tabs `led` field
            // and the mobile remote can never drift. `working` counts fresh PTY
            // output (a `--resume`d session streams a reply with no thinking
            // hook) — see the block comment above for each color's meaning.
            let recent_output = tab.last_output_at.is_some_and(|t| t.elapsed() < STREAMING_LED_WINDOW);
            // Daemon dot (brain/aligator): real liveness from the probe the 2 s
            // persist loop cached — no /proc walk in this per-frame render.
            let is_daemon = matches!(tab.agent_kind.as_deref(), Some("brain" | "aligator"));
            let daemon_alive = crate::daemon_alive_from_probe(tab.daemon_probe.get());
            let agent_led = crate::compute_tab_led(
                tab.agent_state.as_ref().map(|s| s.state),
                session_attached,
                is_daemon,
                agent_alive,
                full_sweep_ran,
                tab.unreviewed_work,
                recent_output,
                daemon_alive,
            )
            .map(|led| {
                let (r, g, b) = led.rgb();
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .mr(px(5.0))
                    .rounded_full()
                    .bg(Hsla::from(Rgba { r, g, b, a: 1.0 }))
            });

            #[cfg(feature = "energy")]
            let power_label = watts.get(i).map(power::TabPower::label).unwrap_or_default();

            // S5: an orchestrator tab gets a lighter background tint so its role
            // reads at a glance in the bar. Role comes from `assignment` (S0),
            // never the volatile `context`.
            let is_orchestrator = crate::api::role_of(tab.assignment.as_deref()) == "orchestrator";
            // Re-home progress badge on a PREDECESSOR tab (S rehome): shows the
            // stage, painted green at safe-to-close. `None` for non-rehoming tabs.
            let rehome_badge = crate::api::rehome_badge(tab.rehome_status.as_deref());
            let drag_name = tab.name.clone();
            let tab_el = div()
                .id(ElementId::Name(self.tab_el_ids[i].clone()))
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
                // Border on all four sides so every tab is fully framed:
                // left/right give the column separators, and top/bottom
                // give each (wrapped) row a horizontal rule — without the
                // bottom line, rows of tabs blurred together vertically.
                // The outer edges sit flush with the bar container's own
                // top/bottom border (same 1px, same colour → one line).
                .border_l_1()
                .border_t_1()
                .border_b_1()
                .bg({
                    let base = if blink_red {
                        tab_blink_bg
                    } else if is_active {
                        tab_active_bg
                    } else {
                        tab_bg
                    };
                    if is_orchestrator {
                        Hsla {
                            l: (base.l + 0.12).min(1.0),
                            ..base
                        }
                    } else {
                        base
                    }
                })
                .border_r_1()
                .border_color(tab_border)
                .text_color(tab_fg)
                .text_size(px(13.0))
                .cursor_pointer()
                // Hover tooltip: the agent-set context (PR/task), if any.
                .when_some(tab.context.clone(), |el, ctx| {
                    el.tooltip(move |_window, cx| {
                        cx.new(|_| TabContextTooltip {
                            text: ctx.to_string(),
                            theme: theme_name,
                        })
                        .into()
                    })
                })
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    if ev.click_count() >= 2 {
                        let name = this.tabs[i].name.to_string();
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
                                        // Rebuild from the live grid — the tab's caches went
                                        // stale while it was backgrounded (see `select_tab`).
                                        app.tabs[i].view.update(cx, |v, vcx| {
                                            v.release_render_caches();
                                            vcx.notify();
                                        });
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
                            link: None,
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
                .when_some(agent_led, ParentElement::child)
                .child(if self.screenshot_censor {
                    // Solid opaque bar over the name — an irreversible redaction
                    // (the text is never drawn), not a reversible blur.
                    div().w(px(72.0)).h(px(14.0)).rounded_sm().bg(tab_fg).into_any_element()
                } else {
                    name.into_any_element()
                });

            // Re-home progress badge: neutral grey through the loop, green at
            // safe-to-close (bidirectional proof done — the predecessor may close).
            let tab_el = tab_el.when_some(rehome_badge, |el, (label, safe)| {
                let (bg, fg) = if safe {
                    (
                        Hsla {
                            h: 0.33,
                            s: 0.5,
                            l: 0.30,
                            a: 1.0,
                        },
                        Hsla {
                            h: 0.33,
                            s: 0.6,
                            l: 0.88,
                            a: 1.0,
                        },
                    )
                } else {
                    (
                        Hsla {
                            h: 0.0,
                            s: 0.0,
                            l: 0.28,
                            a: 1.0,
                        },
                        Hsla {
                            h: 0.0,
                            s: 0.0,
                            l: 0.78,
                            a: 1.0,
                        },
                    )
                };
                el.child(
                    div()
                        .ml(px(6.0))
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded_sm()
                        .bg(bg)
                        .text_color(fg)
                        .text_size(px(10.0))
                        .child(format!("⇄ {label}")),
                )
            });

            #[cfg(feature = "energy")]
            let tab_el = tab_el.child(
                div()
                    .text_size(px(11.0))
                    .text_color(watts_fg)
                    .min_w(px(55.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(power_label),
            );

            // Per-tab RAM mini gauge (#28 S5): a 24 px track with a fill sized by
            // this tab's RSS as a fraction of its ceiling (cap, else system RAM;
            // see the setup above). Fill goes amber past 75% and red past 90% so
            // a tab nearing its cap (OOM) stands out. Only when the pref is on and
            // both a sample and a ceiling exist — off by default so the tab bar
            // stays byte-for-byte unchanged for everyone else.
            let tab_el = match (
                show_tab_gauge.then(|| tab.rss_bytes.get()).flatten(),
                self.tab_mem_ceiling(tab, sys_ram),
            ) {
                (Some(rss), Some(ceiling)) if ceiling > 0 => {
                    let frac = (rss as f32 / ceiling as f32).clamp(0.0, 1.0);
                    let gauge_fill = Hsla::from(ram_gauge_fill(frac));
                    let gauge_track = Hsla::from(Rgba {
                        r: 0.5,
                        g: 0.5,
                        b: 0.5,
                        a: 0.25,
                    });
                    tab_el.child(
                        div()
                            .w(px(24.0))
                            .h(px(4.0))
                            .ml(px(4.0))
                            .rounded_sm()
                            .bg(gauge_track)
                            .child(div().w(px(24.0 * frac)).h(px(4.0)).rounded_sm().bg(gauge_fill)),
                    )
                }
                _ => tab_el,
            };

            // Measure this tab's top edge as a pet ledge (see PetOverlay).
            #[cfg(feature = "pets")]
            // Only measure ledges while pets are actually on screen — this
            // added a canvas element per tab per frame (30-60 fps during
            // floods) for a feature that's usually off. Summoning calls
            // cx.notify(), so ledges appear the same frame the pet does.
            let tab_el = if self.pet.is_active() {
                tab_el.relative().child(self.pet.tab_ledge_canvas(i))
            } else {
                tab_el
            };

            bar = bar.child(tab_el);
        }
        #[cfg(feature = "energy")]
        drop(watts);

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
}
