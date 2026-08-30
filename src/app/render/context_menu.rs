// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `render_context_menu` — the right-click tab menu. Verbatim move (Slice 3, pure move).

#[allow(clippy::wildcard_imports)]
use super::super::*;

impl AppState {
    pub(in crate::app) fn render_context_menu(&self, _window: &Window, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let th = self.th();
        let menu_bg = th.surface_hsla();
        let menu_fg = th.fg_hsla();
        let menu_hover = th.selection_hsla();
        let menu_border = th.border_hsla();

        // Anchor the menu at the click position and let gpui measure the ACTUAL
        // menu height, then flip/clamp so it never runs off-screen. The old code
        // used a hardcoded height estimate (350/400px) computed before the items
        // were even built, so adding menu entries silently made it wrong — the
        // "menu doesn't know its size" bug. `open_upward` now only picks the
        // *base* corner (tab / tab-bar menus grow upward); `anchored`'s default
        // SwitchAnchor still auto-flips it when that direction would overflow.
        let pos = menu.position;

        let mut container = div()
            .id("context-menu")
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
                            let name = this.tabs[idx].name.to_string();
                            this.renaming = Some((idx, name));
                            this.rename_select_all = true;
                            this.context_menu = None;
                            this.rename_focus.focus(window);
                            cx.notify();
                        }),
                    )
                    .child(self.t().rename),
            );

            // Toggle the per-tab RAM mini gauge in the tab bar (#28 S5) and
            // persist the preference so it survives a restart.
            let gauge_label = if self.show_tab_gauge {
                self.t().hide_gauge
            } else {
                self.t().show_gauge
            };
            container = container.child(
                div()
                    .id("menu-toggle-gauge")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.show_tab_gauge = !this.show_tab_gauge;
                            let mut prefs = load_preferences(&platform::config_dir());
                            prefs.show_tab_gauge = this.show_tab_gauge;
                            save_preferences(&platform::config_dir(), &prefs);
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(gauge_label),
            );

            // Copy the tab's working directory to the clipboard.
            // Reads /proc/<pid>/cwd via the platform helper; falls
            // back to the last known cwd captured at spawn time when
            // the live read fails (process gone, /proc unreadable).
            container = container.child(
                div()
                    .id("menu-copy-path")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let pid = this.tabs[idx].view.read(cx).pid();
                            let path = platform::process_cwd(pid).or_else(|| this.tabs[idx].last_known_cwd.clone());
                            if let Some(p) = path {
                                cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_path),
            );

            // Copy a shareable LAN URL — points at the xterm.js viewer
            // (/tabs/by-id/<UUID>/view). UUID rather than tab index so
            // a leaked link is bound to one tab and can't address
            // another by tweaking 0/1/2. The link carries a per-tab
            // share token (not the master api.token): RW for the
            // interactive link, RO for the read-only one. The server
            // refuses RO on `/input` with 403, so the URL *and* the
            // permission level are bound — stripping `&ro=1` does not
            // grant write access. Tokens are minted lazily here on
            // first menu use and persisted via tabs.json so URLs
            // survive restarts.
            for (label, ro) in [(self.t().copy_share_link, false), (self.t().copy_share_link_ro, true)] {
                let port = port_of(&self.api_addr, crate::DEFAULT_API_PORT);
                let tab_id = self.tabs[idx].id.clone();
                let toast_msg = self.t().share_link_copied;
                let id = if ro { "menu-share-link-ro" } else { "menu-share-link" };
                // If the user configured a public base (reverse-proxy
                // URL) use that. Strip any trailing slash so we can
                // unconditionally prepend "/tabs/...".
                let share_base = self.share_url_base.trim_end_matches('/').to_string();
                container = container.child(
                    div()
                        .id(id)
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                // Mint the share token on the runtime Tab if
                                // not yet present so it survives the next
                                // persist tick (snapshot is rebuilt from the
                                // runtime Tab each tick — writing only into
                                // the snapshot would be overwritten in 2s).
                                // Mirror immediately into the snapshot so the
                                // first request against the freshly-copied
                                // URL doesn't 401 during that window.
                                let slot_ref = if ro {
                                    &mut this.tabs[idx].share_token_ro
                                } else {
                                    &mut this.tabs[idx].share_token_rw
                                };
                                if slot_ref.is_empty() {
                                    *slot_ref = crate::mint_share_token().into();
                                }
                                let token = slot_ref.clone();
                                {
                                    let mut snap =
                                        this.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| *t.id == *tab_id) {
                                        if ro {
                                            t.share_token_ro.clone_from(&token);
                                        } else {
                                            t.share_token_rw.clone_from(&token);
                                        }
                                    }
                                }
                                // Resolved on click, not at render time — the
                                // route lookup binds + connects a UDP socket,
                                // and this menu re-renders every frame while
                                // open (two lookups per frame for the RW/RO
                                // pair). On-click also means the copied link
                                // reflects the CURRENT routing table.
                                let base = if share_base.is_empty() {
                                    format!("http://{}:{port}", api::local_ip())
                                } else {
                                    share_base.clone()
                                };
                                let url = if ro {
                                    format!("{base}/tabs/by-id/{tab_id}/view?token={token}&ro=1")
                                } else {
                                    format!("{base}/tabs/by-id/{tab_id}/view?token={token}")
                                };
                                cx.write_to_clipboard(ClipboardItem::new_string(url));
                                let toast_time = std::time::Instant::now();
                                this.toasts.push(Toast {
                                    message: toast_msg.into(),
                                    time: toast_time,
                                    path: None,
                                });
                                this.context_menu = None;
                                cx.notify();
                                // Auto-dismiss after 1s — copy confirmation is
                                // ephemeral; lingering reads as "something
                                // failed".
                                let weak = cx.entity().downgrade();
                                cx.spawn(async move |_, cx: &mut AsyncApp| {
                                    cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                                    let _ = weak.update(cx, |this, cx| {
                                        this.toasts.retain(|t| t.time != toast_time);
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }),
                        )
                        .child(label),
                );
            }

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

            // Re-home: "close the predecessor" — appears ONLY once the
            // bidirectional proof is done (rehome_status == safe-to-close, posted
            // by the old agent on its ACK). Closes the manual-closure gap without
            // removing the human gate: the entry simply isn't there until it's
            // proven safe, and clicking it IS the human's decision.
            if self.tabs.len() > 1 && crate::api::rehome_safe_to_close(self.tabs[idx].rehome_status.as_deref()) {
                let safe_fg = Hsla {
                    h: 0.33,
                    s: 0.6,
                    l: 0.7,
                    a: 1.0,
                };
                container = container.child(
                    div()
                        .id("menu-close-rehome")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .text_color(safe_fg)
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                this.close_tab(idx, cx);
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .child("✓ Fermer le prédécesseur re-home"),
                );
            }

            // Drop catbus-agent into this tab's shell. Ctrl-U clears any
            // half-typed input, then `catbus-agent\n` runs it. No exec —
            // the shell stays alive underneath, so exiting catbus returns
            // the user to their session.
            container = container.child(
                div()
                    .id("menu-catbus")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[idx]
                                .view
                                .read(cx)
                                .send_input_bytes(b"\x15catbus-agent\n".to_vec());
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{1f408}\u{fe0f}\u{1f68c}\u{fe0f} Catbus"),
            );

            // ⛑ Brain — same pattern as Catbus: Ctrl-U + the command +
            // newline, takes over the current tab. Inside the brain
            // tab the user sees the rescue log; the brain watches
            // every OTHER tab via the local HTTP API and POSTs
            // `continue` to any whose scrollback matches a known
            // agent-failure signature OR whose agent_state == "error".
            container = container.child(
                div()
                    .id("menu-brain")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[idx]
                                .view
                                .read(cx)
                                .send_input_bytes(b"\x15tab-atelier brain\n".to_vec());
                            // Flag the tab as the brain watchdog so it
                            // auto-relaunches on restart. Unlike claude/catbus,
                            // brain has no session and never self-announces its
                            // kind, so nothing else would mark it — and the
                            // restore path keys auto-resume off `agent_kind`.
                            // Overwritten if the tab later runs a real agent.
                            this.tabs[idx].agent_kind = Some("brain".into());
                            this.tabs[idx].agent_session_id = None;
                            this.tabs[idx].agent_plan_mode = None;
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{26d1}\u{fe0f} Brain"),
            );

            // 🐊 Aligator — deterministic input router (drains the swamp). Same
            // take-over-the-tab pattern as Brain, and the SAME auto-restart fix:
            // stamp agent_kind=aligator so the restore path relaunches it via
            // `build_agent_resume_command` instead of dropping it to a shell.
            container = container.child(
                div()
                    .id("menu-aligator")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.tabs[idx]
                                .view
                                .read(cx)
                                .send_input_bytes(b"\x15tab-atelier aligator\n".to_vec());
                            // Session-less, like brain — flag the kind so the
                            // daemon auto-relaunches it on restart.
                            this.tabs[idx].agent_kind = Some("aligator".into());
                            this.tabs[idx].agent_session_id = None;
                            this.tabs[idx].agent_plan_mode = None;
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{1f40a} Aligator"),
            );

            // 📊 Dashboard (S5) — opens the harness dashboard scoped to this
            // tab's team, role-aware: a worker/orchestrator drills into its
            // project, a tichef / méta specialist gets the global level 0. Role
            // + project come from `assignment` (S0), never the volatile context.
            // The URL carries the GLOBAL read-only dashboard share token (minted
            // lazily) so it also works from a remote browser.
            {
                let port = port_of(&self.api_addr, crate::DEFAULT_API_PORT);
                let share_base = self.share_url_base.trim_end_matches('/').to_string();
                container = container.child(
                    div()
                        .id("menu-dashboard")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                let token = {
                                    let mut snap =
                                        this.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if snap.dashboard_share_token.is_empty() {
                                        snap.dashboard_share_token = crate::mint_share_token().into();
                                    }
                                    snap.dashboard_share_token.to_string()
                                };
                                let role = crate::api::role_of(this.tabs[idx].assignment.as_deref());
                                let project = crate::api::project_of(
                                    this.tabs[idx].last_known_cwd_string.as_deref(),
                                    this.tabs[idx].assignment.as_deref(),
                                );
                                let base = if share_base.is_empty() {
                                    format!("http://{}:{port}", crate::api::local_ip())
                                } else {
                                    share_base.clone()
                                };
                                let url = crate::api::dashboard_url_for_role(&role, &project, &base, &token);
                                let browser = this.browser.borrow().clone();
                                platform::open_url(&url, browser.as_deref());
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .child("\u{1F4CA} Dashboard"),
                );
            }

            let colors_enabled = self.tabs[idx].view.read(cx).colors_enabled();
            let toggle_label = if colors_enabled {
                self.t().disable_colors
            } else {
                self.t().enable_colors
            };
            container = container.child(
                div()
                    .id("menu-toggle-colors")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                            this.tabs[idx].view.read(cx).set_colors_enabled(!colors_enabled);
                            this.context_menu = None;
                            this.respawn_tab_with_history(idx, window, cx);
                        }),
                    )
                    .child(toggle_label),
            );

            // Lock toggle — flips Tab.locked and pushes the new value
            // into the view so every input path (keyboard, paste,
            // hotkeys, programmatic) refuses immediately. Mirrored
            // into the API snapshot so /input and the share-link
            // viewer both observe the new state without waiting for
            // the next persist tick.
            let locked = self.tabs[idx].locked;
            let lock_label = if locked { self.t().unlock_tab } else { self.t().lock_tab };
            let tab_id_for_lock = self.tabs[idx].id.clone();
            container = container.child(
                div()
                    .id("menu-toggle-lock")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let next = !locked;
                            this.tabs[idx].locked = next;
                            this.tabs[idx].view.read(cx).set_locked(next);
                            {
                                let mut snap = this.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                if let Some(t) = snap.tabs.iter_mut().find(|t| *t.id == *tab_id_for_lock) {
                                    t.locked = next;
                                }
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(lock_label),
            );

            // Internet on/off — flips the tab's bubblewrap net-namespace
            // jail. Set the flag on the view, mirror into the API snapshot
            // (so /tabs and the toggle endpoint agree immediately), then
            // respawn history-preserving so the new netns takes effect —
            // the running shell can't be re-jailed in place. Shown only
            // when bubblewrap is usable, or when the tab is already off
            // (so it can always be turned back on); on a host without
            // bubblewrap and a net-on tab there's nothing to toggle to.
            let net_disabled = self.tabs[idx].view.read(cx).net_disabled();
            if net_disabled || crate::bwrap_available() {
                let net_label = if net_disabled {
                    self.t().enable_internet
                } else {
                    self.t().disable_internet
                };
                let tab_id_for_net = self.tabs[idx].id.clone();
                container = container.child(
                    div()
                        .id("menu-toggle-net")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, window, cx| {
                                let next = !net_disabled;
                                this.tabs[idx].view.read(cx).set_net_disabled(next);
                                {
                                    let mut snap =
                                        this.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| *t.id == *tab_id_for_net) {
                                        t.net_disabled = next;
                                    }
                                }
                                this.context_menu = None;
                                this.respawn_tab_with_history(idx, window, cx);
                            }),
                        )
                        .child(net_label),
                );
            }

            // (Background-color + Schedule preset rows used to live
            // here but pushed the context menu taller than a small-
            // laptop viewport. Both settings have CLI entry points
            // that scale better:
            //   tab-atelier-headless bg-color <tab> #RRGGBB
            //   tab-atelier-headless schedule <tab> "Mo-Fr 9-18" --tz …
            // The global Theme picker stays in the Preferences modal.)
        }

        {
            let stats_idx = match menu.kind {
                MenuKind::Tab(idx) => idx,
                MenuKind::Background => self.active,
            };
            let stat_fg = th.fg_muted_hsla();
            let elapsed = self.tabs[stats_idx].uptime();
            let t = self.t();

            let mut stats_lines: Vec<String> = Vec::new();

            #[cfg(feature = "energy")]
            {
                let power_info = self
                    .power_watts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(stats_idx)
                    .cloned();
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
            }
            stats_lines.push(format!("{}: {}", t.uptime, format_duration(elapsed)));
            // How long since this tab was last the foreground tab. The active
            // tab reads ~0 (refreshed every sweep); background tabs age.
            if let Some(seen) = self.tabs[stats_idx].last_focused_at {
                stats_lines.push(format!("{}: {}", t.last_seen, format_duration(seen.elapsed())));
            }
            // Per-tab consumption (issue #28): resident memory of the shell
            // subtree + last agent token totals. Sampled once here, on popup
            // open — not per frame. GUI renders memory in MB.
            let shell_pid = self.tabs[stats_idx].view.read(cx).pid();
            if let Some(sample) = crate::agent_probe::sample_tree(shell_pid) {
                let mb = sample.rss_kb as f64 / 1024.0;
                stats_lines.push(format!("{}: {mb:.0} MB", t.memory));
            }
            #[cfg(feature = "catbus")]
            if let Some(usage) = self.tabs[stats_idx].tokens_last_saved.get() {
                stats_lines.push(format!("{}: {} in / {} out", t.tokens, usage.input, usage.output));
            }
            let conns = self
                .tab_connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&*self.tabs[stats_idx].id)
                .copied()
                .unwrap_or(0);
            if conns > 0 {
                stats_lines.push(format!("{}: {conns}", t.connections));
            }

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
        container = container.child(sep());
        // "Copy path (link)" — shown only when the right-click landed on a
        // detected link (populated on the terminal-area menu). Copies the raw
        // URL/path text to the system clipboard.
        if let Some(link) = menu.link.clone() {
            container = container.child(
                div()
                    .id("menu-copy-link")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_link),
            );
        }
        container = container
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
                            // Clipboard gets plain text so other apps don't see
                            // raw `\x1b[...m` escapes. The persistence call
                            // sites that need colours go through copy_all_history
                            // directly.
                            let text = crate::strip_ansi(&this.tabs[this.active].view.read(cx).copy_all_history());
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().copy_all),
            );

        // Paste row — XOR'd by whether the active tab has a live
        // selection.
        //
        // No selection ⇒ surface "Paste" (system clipboard). Useful
        // for piping commands back in from a separate editor.
        //
        // Selection present ⇒ surface "Paste selection" instead and
        // suppress plain "Paste". The user just highlighted something
        // they want to act on — offering both reads as a near-miss
        // (one wrong click and you've overwritten the clipboard with
        // an unrelated paste), so we collapse to the single
        // contextually-correct action.
        let has_active_selection = self.tabs[self.active].view.read(cx).has_selection();
        if has_active_selection {
            container = container.child(
                div()
                    .id("menu-paste-selection")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            let view = &this.tabs[this.active].view;
                            if let Some(text) = view.read(cx).copy_selection() {
                                view.read(cx).send_clipboard(&text);
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().paste_selection),
            );
        } else {
            container = container.child(
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
                                && let Some(text) = TerminalView::clipboard_to_paste_text(&item)
                            {
                                let view = &this.tabs[this.active].view;
                                view.read(cx).send_clipboard(&text);
                            }
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(self.t().paste),
            );
        }
        container = container
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
                            this.do_screenshot(ScreenshotMode::Tab, cx);
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
                            this.do_screenshot(ScreenshotMode::App, cx);
                        }),
                    )
                    .child(self.t().screenshot_app),
            )
            .child(
                div()
                    .id("menu-screenshot-redacted")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            this.do_screenshot(ScreenshotMode::Redacted, cx);
                        }),
                    )
                    .child(self.t().screenshot_redacted),
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
            // Only shown when at least one agent tab is dead (dim-red LED) — a
            // one-click "bring back every agent whose process died" that respawns
            // each on its persisted `--resume` session.
            .when(self.dead_agent_count() > 0, |c| {
                let label = format!("⟳ {} ({})", self.t().relaunch_dead_agents, self.dead_agent_count());
                c.child(
                    div()
                        .id("menu-relaunch-dead")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                                this.relaunch_dead_agents(window, cx);
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .child(label),
                )
            })
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
            // Claude-only mode. When off, this enables it (new tabs launch
            // `claude` in auto mode). When on, it flips to a "New bash tab"
            // escape hatch that cancels the mode AND opens a plain shell.
            .child({
                let label = if self.claude_only {
                    "🐚 New bash tab"
                } else {
                    "🤖 Claude-only mode"
                };
                div()
                    .id("menu-claude-only")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                            if this.claude_only {
                                // Cancel the mode, then open a shell tab (now
                                // that force-claude is off, `add_tab` yields bash).
                                this.set_claude_only_mode(false);
                                this.context_menu = None;
                                this.add_tab(window, cx);
                            } else {
                                this.set_claude_only_mode(true);
                                this.context_menu = None;
                            }
                            cx.notify();
                        }),
                    )
                    .child(label)
            })
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
                            this.qr_modal = this.build_qr_modal_data();
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
                        cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                            this.pref_browser_text = this.browser.borrow().clone().unwrap_or_default();
                            this.pref_editor_text = this.code_editor.borrow().clone().unwrap_or_default();
                            this.pref_api_addr_text = this.api_addr.clone();
                            this.pref_api_tls_addr_text = this.api_tls_addr.clone();
                            this.pref_share_url_base_text = this.share_url_base.clone();
                            this.pref_default_mem_text = this.default_tab_mem_max.clone().unwrap_or_default();
                            this.show_preferences = true;
                            this.context_menu = None;
                            // Move focus into the first prefs input on
                            // open so the user can type immediately
                            // without clicking. Without this, the
                            // terminal still has focus and the inputs
                            // *appear* unfocusable because their
                            // on_mouse_down focus call fires AFTER
                            // gpui dispatches the first click's keys
                            // — by which point the keys are already
                            // queued at the terminal.
                            this.pref_api_addr_focus.focus(window);
                            cx.notify();
                        }),
                    )
                    .child(self.t().preferences),
            );

        // Screen-mate pets (background menu): "Summon" adds one more to the herd;
        // "Dismiss all" appears only when at least one pet is on screen.
        #[cfg(feature = "pets")]
        {
            container = container.child(sep()).child(
                div()
                    .id("menu-pet-summon")
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(menu_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                            this.summon_pet(window, cx);
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("🐾 Summon a pet"),
            );
            if self.pet.count() > 0 {
                container = container.child(
                    div()
                        .id("menu-pet-dismiss")
                        .px(px(12.0))
                        .py(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(menu_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                this.pet.dismiss_all();
                                this.context_menu = None;
                                cx.notify();
                            }),
                        )
                        .child("🐾 Dismiss all pets"),
                );
            }
        }

        // Base corner from the persisted hint; anchored auto-switches it on
        // overflow after measuring the real height.
        let anchor = if menu.open_upward {
            gpui::Corner::BottomLeft
        } else {
            gpui::Corner::TopLeft
        };
        Some(
            gpui::anchored()
                .position(pos)
                .anchor(anchor)
                .child(container)
                .into_any_element(),
        )
    }
}
