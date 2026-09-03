// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `AppState::persist` — the GUI's 2 s persist tick, relocated here VERBATIM
//! from `app/mod.rs` (Slice 2B, pure move: byte-identical body via an inherent
//! `impl AppState` block in this child module; only visibility/imports adapted).
//!
//! ponytail: still one monolith. The entangled derivation units inside it
//! (`sample_tab_metrics` = per-tab RSS + /proc daemon-probe, and `build_snapshot`
//! = the `SnapshotTab` derivation) are NOT split here — their seams cross the
//! `api_tabs` loop's shared locals + the live `tab.view.read(cx)` borrow, so a
//! faithful split would change call order / touch /proc sampling, which the 2A
//! serialization net does NOT cover. Deferred to a dedicated follow-up that
//! builds a DERIVATION net (Gate A on the logic) first.

// Verbatim relocation → keep the parent's whole scope rather than re-listing
// dozens of (partly `cfg`-gated) symbols the moved body already used by name.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    pub(super) fn persist(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            let tab = &mut self.tabs[self.active];
            let idle = tab
                .view
                .read(cx)
                .last_input_time()
                .is_none_or(|t| t.elapsed().as_secs() >= 30);
            if idle && tab.last_activated.is_some() {
                tab.deactivate();
            } else if !idle && tab.last_activated.is_none() {
                tab.activate();
            }
        }
        #[cfg(feature = "energy")]
        {
            let watts = self
                .power_watts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if let Some(w) = watts.get(i).and_then(|p| p.watts) {
                    tab.energy_wh += w * 2.0 / 3600.0;
                }
            }
        }
        let state_base = platform::state_base_dir();
        // Refresh last_known_cwd for any tab whose PTY child is still alive,
        // so a later persist tick after the shell exits still has a value
        // to fall back on instead of blanking the cwd to None. Update the
        // stringified mirror only when the PathBuf actually changed, so
        // unchanged tabs allocate nothing here. Gated on ring movement:
        // the shell's cwd only changes via `cd`, whose prompt redraw emits
        // bytes — a silent-since-last-tick tab skips the /proc readlink
        // (which every tab paid every 2 s forever).
        for tab in &mut self.tabs {
            let ring_len = tab.view.read(cx).ring_len();
            // Dormant-LED stamp: the ring grew ⇒ the tab produced output
            // (claude streaming / a build printing). Tracked on its own
            // memo — decoupled from the snapshot cache, which no longer
            // refreshes while nobody consumes the API.
            if ring_len != tab.led_last_ring.get() {
                tab.led_last_ring.set(ring_len);
                tab.last_output_at = Some(std::time::Instant::now());
            }
            if tab.snap_cache.as_ref().is_some_and(|c| c.ring_len == ring_len) {
                continue;
            }
            let pid = tab.view.read(cx).pid();
            if let Some(p) = platform::process_cwd(pid)
                && tab.last_known_cwd.as_deref() != Some(p.as_path())
            {
                tab.last_known_cwd_string = Some(p.to_string_lossy().into());
                tab.last_known_cwd = Some(p);
            }
        }
        // Track the API activity signal so persist-tick work that only
        // serves API consumers can be skipped while nobody is connected.
        {
            let seq = self.activity_signal.load(std::sync::atomic::Ordering::Relaxed);
            if seq != self.activity_last_seen.get() {
                self.activity_last_seen.set(seq);
                self.activity_last_at.set(Some(std::time::Instant::now()));
            }
        }
        let api_hot = self.activity_last_at.get().is_some_and(|t| t.elapsed().as_secs() < 60);
        // Keep the power sampler fast only while its numbers are visible
        // somewhere (tab bar on screen, or an API consumer polling).
        #[cfg(feature = "energy")]
        self.power_hot
            .store(self.visible || api_hot, std::sync::atomic::Ordering::Relaxed);
        // Connection metering (throttled ~5 s — the /proc scan is too heavy
        // for every 2 s persist tick). Desktop is unprivileged, so it's
        // connections only (no nft byte counters). Two more gates:
        //  - only when the numbers can be SEEN — the context menu's stats
        //    block or an API consumer that's been active within a minute.
        //    Idle with nothing open ⇒ zero /proc scans.
        //  - the scan runs on the background executor; it stats every
        //    process on the host and used to stall the main thread 10-50 ms.
        #[cfg(target_os = "linux")]
        if (api_hot || self.context_menu.is_some())
            && self.last_conn_meter.get().is_none_or(|t| t.elapsed().as_secs() >= 5)
        {
            self.last_conn_meter.set(Some(std::time::Instant::now()));
            let roots: Vec<(String, u32)> = self
                .tabs
                .iter()
                .map(|tab| (tab.id.to_string(), tab.view.read(cx).pid()))
                .collect();
            let out = self.tab_connections.clone();
            cx.background_executor()
                .spawn(async move {
                    let counts = crate::net_meter::connection_counts(&roots);
                    *out.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = counts;
                })
                .detach();
        }
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let cwd = tab.last_known_cwd_string.as_deref().map(str::to_string);
                TabState {
                    id: tab.id.to_string(),
                    name: tab.name.to_string(),
                    cwd,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                    net_disabled: tab.view.read(cx).net_disabled(),
                    agent_session_id: tab.agent_session_id.as_deref().map(str::to_string),
                    agent_kind: tab.agent_kind.as_deref().map(str::to_string),
                    spawn_mode: tab.spawn_mode, // SV4: persist the A/B partition key
                    agent_plan_mode: tab.agent_plan_mode,
                    tab_env: tab.tab_env.clone(),
                    pinned_cols: tab.pinned_cols,
                    pinned_rows: tab.pinned_rows,
                    share_token_rw: tab.share_token_rw.to_string(),
                    share_token_ro: tab.share_token_ro.to_string(),
                    locked: tab.locked,
                    schedule: tab.schedule.clone(),
                    bg_color: tab.bg_color.clone(),
                    assignment: tab.assignment.as_deref().map(str::to_string),
                    specialty: tab.specialty.as_deref().map(str::to_string),
                    orchestrator: tab.orchestrator.as_deref().map(str::to_string),
                    objective: tab.objective.as_deref().map(str::to_string),
                    current_task: tab.current_task.clone(),
                    rounds_active: tab.rounds_active.clone(),
                    evaluations: tab.evaluations.clone(),
                    usage_count: tab.usage_count,
                    last_used_at: tab.last_used_at,
                    conventions: tab.conventions.clone(),
                    parent_tab_id: tab.parent_tab_id.as_deref().map(str::to_string),
                    rehome_status: tab.rehome_status.as_deref().map(str::to_string),
                    limits: tab.limits.clone(),
                    ..TabState::default()
                }
            })
            .collect();
        // Anyone actually consuming the API snapshot? With no recent
        // authenticated request and no WS viewer, grid scans and
        // SnapshotTab rebuilds produce data nobody reads — staleness is
        // invisible until a consumer returns, and their first request
        // flips `api_hot` so the next 2 s tick catches up.
        let api_consumers = api_hot || self.tabs.iter().any(|tab| tab.view.read(cx).viewer_count() > 0);
        let mut api_tabs: Vec<api::SnapshotTab> = Vec::with_capacity(self.tabs.len());
        // Loop-invariant consumer gate — `filter` keeps the long body
        // un-reindented; with no consumers the loop runs zero times.
        for (tab, ts) in self.tabs.iter_mut().zip(tabs.iter()).filter(|_| api_consumers) {
            let view = tab.view.read(cx);
            let shell_pid = view.pid();
            let pty_ring = view.pty_ring();
            // Dirtiness key: bytes ever written through the PTY ring.
            // Unchanged ⇒ the grid is byte-identical, so skip the scans.
            let ring_len = view.ring_len();
            // 200 lines for the joined `output` (logical lines — the
            // mobile remote word-wraps them, more is wasted bandwidth on
            // a phone screen). 2000 for `raw_output` so xterm.js's
            // scrollback has actual history to browse through.
            // The 2000-line `raw_output` (xterm.js scrollback) is only consumed
            // when someone is actually web-viewing THIS tab. For an unwatched
            // tab — the common case, e.g. 20 background agents streaming while
            // you watch one — skip that scan and keep only the cheap 200-line
            // `output` the /tabs list needs. When a viewer attaches to a tab
            // whose cache was built without raw scrollback, refresh once so
            // xterm.js still gets history (within one persist tick).
            let want_raw = view.viewer_count() > 0;
            let stale = tab.snap_cache.as_ref().is_none_or(|c| c.ring_len != ring_len);
            let needs_raw_backfill = want_raw
                && tab
                    .snap_cache
                    .as_ref()
                    .is_some_and(|c| c.raw_output.is_empty() && !c.output.is_empty());
            // Last viewer detached and the tab then went quiet: the dump
            // would normally be dropped by the next rebuild-on-output, but
            // a silent tab would pin megabytes of scrollback text nobody
            // can read anymore. Shed it without rescanning the grid.
            let needs_raw_drop = !want_raw && tab.snap_cache.as_ref().is_some_and(|c| !c.raw_output.is_empty());
            let fresh = if stale || needs_raw_backfill {
                let (output, cursor) = view.ansi_text_with_cursor(Some(200));
                let (raw_output, raw_cursor) = if want_raw {
                    view.raw_screen_text(Some(2000))
                } else {
                    (String::new(), None)
                };
                let (cols, rows) = view.dims();
                Some(crate::term_export::GridSnapshotCache::new(
                    ring_len, output, cursor, raw_output, raw_cursor, cols, rows,
                ))
            } else if needs_raw_drop {
                tab.snap_cache
                    .as_ref()
                    .map(crate::term_export::GridSnapshotCache::without_raw)
            } else {
                None
            };
            // No further use of `view` past here, so the borrow of
            // `tab.view` ends and we can mutate `tab.snap_cache`.
            if let Some(c) = fresh {
                tab.snap_cache = Some(c);
            }
            // Populated just above; if somehow absent, skip this tab in the
            // snapshot this tick rather than panic (next tick refills it).
            let Some(grid) = tab.snap_cache.clone() else {
                continue;
            };
            // Inc9 b2/b3: context-% used from the screen + brutal-drop (compaction)
            // detection between ticks. Cell fields → updatable through `&tab`.
            let ctx_pct = crate::cli::clarify::parse_context_pct(&grid.output);
            if crate::cli::clarify::detect_compaction(tab.last_context_pct.get(), ctx_pct) {
                tab.last_compaction_at.set(Some(crate::unix_millis()));
            }
            if ctx_pct.is_some() {
                tab.last_context_pct.set(ctx_pct);
            }
            let bg_color = crate::effective_tab_bg(tab.bg_color.as_deref(), self.tab_bg_global.as_deref()).into();
            // Per-tab RSS (#28 S1/S5): one /proc-subtree walk at the 2 s persist
            // cadence, cached on the tab for the tab-bar gauge and mirrored to
            // the snapshot below.
            let rss_bytes = crate::agent_probe::sample_tree_cached(shell_pid).map(|s| s.rss_kb.saturating_mul(1024));
            tab.rss_bytes.set(rss_bytes);
            // Daemon-liveness probe (brain/aligator) at the same 2 s cadence,
            // cached so the per-frame tab-strip dot and the /dashboard snapshot
            // both read it without walking /proc. Only daemons pay the walk.
            if matches!(tab.agent_kind.as_deref(), Some("brain" | "aligator")) {
                tab.daemon_probe.set(crate::agent_probe::subtree_has_daemon(
                    shell_pid,
                    tab.agent_kind.as_deref().unwrap_or(""),
                ));
            }
            // Fold in the ring's viewer-attach timestamp: a viewer (browser /
            // mobile remote) opening the tab stamped it at connect time, so the
            // open is recorded reliably even if the view already closed — a
            // polled viewer_count edge missed those. Monotonic: only advances.
            let attached = pty_ring.lock().map_or(0, |r| r.viewer_attached_at_millis());
            if attached > tab.last_used_at.unwrap_or(0) {
                tab.last_used_at = Some(attached);
            }
            api_tabs.push(api::SnapshotTab {
                id: tab.id.clone(),
                name: tab.name.clone(),
                cwd: tab.last_known_cwd_string.clone(),
                // ANSI escapes are kept so the mobile remote can render
                // colours instead of the previous flat-grey text.
                output: grid.output,
                raw_output: grid.raw_output,
                output_crc: grid.output_crc,
                raw_output_crc: grid.raw_output_crc,
                raw_cursor: grid.raw_cursor,
                uptime_secs: tab.uptime().as_secs_f64(),
                cursor: grid.cursor,
                cols: grid.cols,
                rows: grid.rows,
                share_token_rw: tab.share_token_rw.clone(),
                share_token_ro: tab.share_token_ro.clone(),
                locked: ts.locked,
                schedule: ts.schedule.clone(),
                bg_color,
                context: tab.context.clone(),
                assignment: tab.assignment.clone(),
                parent_tab_id: tab.parent_tab_id.clone(),
                rehome_status: tab.rehome_status.clone(),
                shell_pid,
                agent_state: tab.agent_state.clone(),
                agent_session_id: tab.agent_session_id.clone(),
                agent_kind: tab.agent_kind.clone(),
                spawn_mode: tab.spawn_mode,
                // Derive the LED once here (same inputs the tab-strip renderer
                // uses) so /tabs and the mobile remote match the desktop dot.
                agent_led: {
                    #[cfg(feature = "catbus")]
                    let (agent_alive, full_sweep_ran) = (
                        tab.agent_pid.get().is_some(),
                        self.last_agent_full_sweep.get().is_some(),
                    );
                    #[cfg(not(feature = "catbus"))]
                    let (agent_alive, full_sweep_ran) = (true, false);
                    let recent_output = tab.last_output_at.is_some_and(|t| t.elapsed() < STREAMING_LED_WINDOW);
                    // Daemon LED (brain/aligator): real liveness from the cached
                    // /proc-subtree probe (set just above) — up=green, down=red.
                    let is_daemon = matches!(tab.agent_kind.as_deref(), Some("brain" | "aligator"));
                    let daemon_alive = crate::daemon_alive_from_probe(tab.daemon_probe.get());
                    crate::compute_tab_led(
                        tab.agent_state.as_ref().map(|s| s.state),
                        tab.agent_kind.is_some(),
                        is_daemon,
                        agent_alive,
                        full_sweep_ran,
                        tab.unreviewed_work,
                        recent_output,
                        daemon_alive,
                    )
                },
                last_used_at: tab.last_used_at,
                viewers: pty_ring.lock().map_or(0, |r| r.viewer_count()),
                pty_ring: Some(pty_ring),
                net_disabled: ts.net_disabled,
                connections: self
                    .tab_connections
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&*tab.id)
                    .copied()
                    .unwrap_or(0),
                // Desktop is unprivileged → no nft byte counters.
                tx_bytes: 0,
                tx_denied_bytes: 0,
                // Desktop allowlist isn't wired (headless-only feature).
                net_allow: crate::net_policy::AllowConfig::default(),
                dns_entries: Vec::new(),
                // Per-tab consumption (issue #28): RSS sampled just above (also
                // cached on the tab for the tab-bar gauge), token total mirrored
                // from the tab.
                resident_memory_bytes: rss_bytes,
                // Token totals come from the catbus sidecar cache; without the
                // `catbus` feature (e.g. the Windows GUI build) there's no such
                // field, so the tab reports no tokens.
                #[cfg(feature = "catbus")]
                tokens: tab.tokens_last_saved.get(),
                #[cfg(not(feature = "catbus"))]
                tokens: None,
                specialty: tab.specialty.clone(),
                orchestrator: tab.orchestrator.clone(),
                objective: tab.objective.clone(),
                current_task: tab.current_task.clone(),
                rounds_active: tab.rounds_active.clone(),
                evaluations: tab.evaluations.clone(),
                usage_count: tab.usage_count,
                conventions: tab.conventions.clone(),
                // Inc9 b2/b3: computed just above from the screen (used %) and
                // the cross-tick brutal-drop stamp (compaction/rehome).
                context_pct: ctx_pct,
                last_compaction_at: tab.last_compaction_at.get(),
            });
        }

        let read_only = crate::read_only();
        // Persist the global dashboard share-token (lives on the API snapshot,
        // like the master token) so a shared /dashboard link survives a restart.
        let dashboard_share_token = self
            .api_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dashboard_share_token
            .to_string();
        let saved = SavedState {
            tabs,
            active: self.active,
            windowed: self.windowed,
            dashboard_share_token,
        };
        // Skip the write+rotate when the serialized content is identical to
        // last tick — the common case once the user stops poking the UI.
        // The string serialized for the hash IS what gets written, so the
        // dirty path doesn't serialize the same value a second time.
        let serialized = serde_json::to_string_pretty(&saved).unwrap_or_default();
        let new_hash = crate::crc32(serialized.as_bytes());
        if !read_only && new_hash != self.last_state_hash.get() {
            self.last_state_hash.set(new_hash);
            // Off-thread: the atomic write ends in an fsync that can
            // stall tens of ms — a keystroke landing mid-persist froze
            // for it (issue #9).
            let config_base = platform::config_base_dir();
            self.state_writer
                .submit(move || crate::save_state_serialized(&config_base, &serialized));
        }
        if !read_only {
            // Hand the scrollback-save off to the worker thread: build a cheap
            // job per tab (name + ring_len + a `Send` serialize closure) and
            // submit. The worker does the ring/crc dirtiness gate and the
            // expensive `copy_all_history` + disk write, so this tick never
            // stalls typing on a full-grid serialize (the old inline cost).
            let batch: Vec<SaveJob> = self
                .tabs
                .iter()
                .map(|tab| {
                    let view = tab.view.read(cx);
                    SaveJob {
                        name: tab.name.clone(),
                        ring_len: view.ring_len(),
                        serialize: Box::new(view.history_job(Some(crate::PERIODIC_OUTPUT_SAVE_LINES))),
                    }
                })
                .collect();
            self.output_saver.submit(batch);
        }
        // Uptime + energy are never written in read-only mode; in normal
        // mode each has its own throttle (30s for uptime, ≥0.1 Wh delta for
        // energy) plus an unconditional flush on shutdown.
        if !read_only {
            let should_save_uptime = self
                .last_uptime_save
                .get()
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(30));
            if should_save_uptime {
                for tab in &self.tabs {
                    let secs = tab.uptime().as_secs_f64();
                    // Deactivated tabs' uptime is frozen — skip the atomic
                    // rewrite of an identical value (N-1 of N tabs, every 30 s).
                    if tab.uptime_last_saved.get() != Some(secs.to_bits()) {
                        tab.uptime_last_saved.set(Some(secs.to_bits()));
                        let base = state_base.clone();
                        let name = tab.name.clone();
                        self.state_writer.submit(move || save_tab_uptime(&base, &name, secs));
                    }
                }
                self.last_uptime_save.set(Some(std::time::Instant::now()));
            }
            #[cfg(feature = "energy")]
            {
                const ENERGY_DELTA_WH: f64 = 0.1;
                for tab in &mut self.tabs {
                    if (tab.energy_wh - tab.energy_wh_last_saved).abs() >= ENERGY_DELTA_WH {
                        tab.energy_wh_last_saved = tab.energy_wh;
                        let base = state_base.clone();
                        let name = tab.name.clone();
                        let wh = tab.energy_wh;
                        self.state_writer.submit(move || save_tab_energy(&base, &name, wh));
                    }
                }
            }
            // Token usage: read the sidecar written by catbus-agent and
            // persist it to the standard per-tab state file so the rest of
            // the app (and the mobile remote) can read cumulative totals
            // without knowing about the ~/.claude/projects layout.
            //
            // `find_session` is a full /proc subtree walk per tab. Tabs
            // with an attached agent session refresh every tick; tabs
            // WITHOUT one are only probed for discovery (a claude launched
            // by hand, no hooks) every ~30 s — a plain shell almost never
            // grows an agent between ticks, so walking its subtree 30×/min
            // was pure overhead.
            #[cfg(feature = "catbus")]
            let discover = self
                .last_token_discovery
                .get()
                .is_none_or(|t| t.elapsed().as_secs() >= 30);
            #[cfg(feature = "catbus")]
            if discover {
                self.last_token_discovery.set(Some(std::time::Instant::now()));
            }
            #[cfg(feature = "catbus")]
            for tab in &self.tabs {
                if tab.agent_kind.is_none() && !discover {
                    continue;
                }
                // Token counters only move when the agent finishes a
                // prompt, which always prints — a tab whose ring hasn't
                // advanced can't have new totals. The 30 s discovery
                // beat doubles as failsafe.
                let ring_len = tab.view.read(cx).ring_len();
                if !discover && ring_len == tab.tokens_last_ring.get() {
                    continue;
                }
                tab.tokens_last_ring.set(ring_len);
                // Reuse the LED sweep's subtree walk when it already
                // located the agent; fall back to the full walk for
                // discovery (non-agent tabs / first tick after attach).
                let session = tab.agent_pid.get().map_or_else(
                    || crate::catbus_agent::find_session(tab.view.read(cx).pid()),
                    crate::catbus_agent::find_session_for,
                );
                if let Some(session) = session
                    && let Some(usage) = crate::catbus_agent::read_session_tokens(&session)
                    // Usage is cumulative and only moves when the agent
                    // finishes a prompt — skip the (double-fsync) rewrite
                    // of an identical ~40-byte file on all other ticks.
                    && tab.tokens_last_saved.get() != Some(usage)
                {
                    tab.tokens_last_saved.set(Some(usage));
                    // The double fsync (file + dir) is exactly the stall
                    // the writer thread exists for.
                    let base = state_base.clone();
                    let name = tab.name.clone();
                    self.state_writer.submit(move || save_tab_tokens(&base, &name, &usage));
                }
            }
        }

        // A SIGINT/SIGTERM came in; do the unconditional flush and quit.
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            log::info!("graceful shutdown requested by signal, flushing state");
            self.close_all_tabs(cx);
            return;
        }

        // A hot-swap upgrade came in (`POST /upgrade`): flush all state,
        // then replace this process with the (re)installed binary at our
        // own path, handing every tab's live PTY across the exec — the
        // shells never notice. Returns only if the exec failed. (#23 —
        // carried into the peeled persist from this fork's fabric context;
        // `hot_swap`/`flush_all_state` stay in app/mod.rs.)
        #[cfg(unix)]
        if crate::hotswap::upgrade_requested() && !crate::read_only() {
            self.hot_swap(cx);
            return;
        }

        // Skipped entirely while nobody consumes the API — the previous
        // snapshot stays in place (never wiped with an empty one) and
        // the first request after idle serves it, at most 2 s + idle
        // staleness, then flips `api_hot` for the next tick.
        if api_consumers {
            let mut snapshot = self.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.tabs = api_tabs;
            snapshot.active = self.active;
            // Invalidate the /tabs cache (next GET rebuilds once) and bump
            // the meta generation so WS meta ticks rebuild.
            snapshot.invalidate_tabs();
            #[cfg(feature = "energy")]
            snapshot.power.clone_from(
                &self
                    .power_watts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            #[cfg(feature = "energy")]
            {
                snapshot.battery_percent = *self
                    .battery_percent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        #[cfg(feature = "energy")]
        {
            let pids: Vec<u32> = self.tabs.iter().map(|tab| tab.view.read(cx).pid()).collect();
            *self
                .power_pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = pids;
        }

        {
            let mut snapshot = self.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut closes: Vec<usize> = snapshot.pending_closes.drain(..).collect();
            let activate = snapshot.pending_activate.take();
            let inputs: Vec<(usize, Vec<u8>)> = snapshot.pending_input.drain(..).collect();
            let renames: Vec<(usize, String)> = snapshot.pending_renames.drain(..).collect();
            let status_updates: Vec<api::PendingStatusUpdate> = snapshot.pending_status_updates.drain(..).collect();
            let lock_changes: Vec<(String, bool)> = snapshot.pending_lock_changes.drain(..).collect();
            let net_changes: Vec<(String, bool)> = snapshot.pending_net_changes.drain(..).collect();
            let bg_color_changes: Vec<(String, Option<String>)> = snapshot.pending_bg_color_changes.drain(..).collect();
            let context_changes: Vec<(String, Option<String>)> = snapshot.pending_context_changes.drain(..).collect();
            let assignment_changes: Vec<(String, Option<String>)> =
                snapshot.pending_assignment_changes.drain(..).collect();
            let card_changes: Vec<(String, crate::api::CardChange)> = snapshot.pending_card_changes.drain(..).collect();
            let parent_changes: Vec<(String, Option<String>)> = snapshot.pending_parent_changes.drain(..).collect();
            let rehome_changes: Vec<(String, Option<String>)> = snapshot.pending_rehome_changes.drain(..).collect();
            let token_rotations: Vec<String> = snapshot.pending_token_rotations.drain(..).collect();
            let schedule_changes: Vec<(String, Option<crate::schedule::TabSchedule>)> =
                snapshot.pending_schedule_changes.drain(..).collect();
            let limit_changes: Vec<(String, crate::TabResourceLimits, bool)> =
                snapshot.pending_limit_changes.drain(..).collect();
            let default_limit_change: Option<(crate::TabResourceLimits, bool)> = snapshot.pending_default_limits.take();
            let resize_changes: Vec<(String, Option<(u16, u16)>)> = snapshot.pending_resizes.drain(..).collect();
            let claude_only_change: Option<bool> = snapshot.pending_claude_only.take();
            let relay_mode_change: Option<bool> = snapshot.pending_relay_mode.take();
            let relay_config_change = snapshot.pending_relay_config.take();
            let env_changes: Vec<crate::api::EnvChange> = snapshot.pending_env_changes.drain(..).collect();
            drop(snapshot);
            // Relay-mode toggle from the CLI/API (`relay on|off`).
            if let Some(on) = relay_mode_change {
                self.set_relay_mode_mode(on);
            }
            // Relay endpoint/egress change (`relay via` / `relay egress`).
            if let Some(ch) = relay_config_change {
                crate::apply_relay_config(&ch, &platform::config_dir());
            }
            // Env changes (`env set/unset`). Global → the process-global map +
            // preference; per-tab → the runtime Tab (persisted next tick). Both
            // apply on the tab's next (re)spawn.
            for ch in env_changes {
                if let Some(id) = ch.tab {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == id) {
                        for (k, v) in ch.set {
                            tab.tab_env.insert(k, v);
                        }
                        for k in ch.unset {
                            tab.tab_env.remove(&k);
                        }
                    }
                } else {
                    let mut g = crate::tab_env_global();
                    for (k, v) in ch.set {
                        g.insert(k, v);
                    }
                    for k in ch.unset {
                        g.remove(&k);
                    }
                    crate::set_tab_env_global(g.clone());
                    if !crate::read_only() {
                        let mut prefs = load_preferences(&platform::config_dir());
                        prefs.tab_env = g;
                        save_preferences(&platform::config_dir(), &prefs);
                    }
                }
            }
            // Forced Claude-only toggle from the CLI/API (`claude-only on|off`).
            // Mirror onto the struct field + global (read by `insert_tab`) and
            // persist, so the change survives a restart like the menu toggle.
            if let Some(on) = claude_only_change {
                self.set_claude_only_mode(on);
            }
            // Per-tab fixed-size pins (`tab-atelier resize`): set the view's
            // pinned grid (applies immediately) + mirror onto the runtime Tab so
            // the next persist tick writes it to tabs.json. `None` un-pins.
            for (tab_id, dims) in resize_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    let grid = dims.map(|(c, r)| (c as usize, r as usize));
                    tab.view.update(cx, |v, _| v.set_pinned_grid(grid));
                    tab.pinned_cols = dims.map(|(c, _)| c);
                    tab.pinned_rows = dims.map(|(_, r)| r);
                }
            }
            // Apply lock toggles from the API/CLI onto the runtime
            // Tab's manual flag. The view's set_locked() push happens
            // in the per-tick mirror below — that's the single site
            // that funnels `effective_locked()` into the gpui view,
            // so a future caller can't accidentally toggle the view
            // without also covering schedule-driven locks.
            for (tab_id, locked) in lock_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.locked = locked;
                }
            }
            // Net on/off toggles from the API/CLI. Set the view's flag
            // and respawn the PTY so the bubblewrap netns jail takes
            // effect — the shell can't be re-jailed in place. No window
            // here (persist tick), so use the low-level respawn rather
            // than `respawn_tab_with_history`; refocus isn't needed for a
            // background toggle.
            for (tab_id, disabled) in net_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    let cwd = platform::process_cwd(tab.view.read(cx).pid()).or_else(|| std::env::current_dir().ok());
                    tab.view.update(cx, |v, _| {
                        v.set_net_disabled(disabled);
                        v.respawn(cwd.as_deref());
                    });
                }
            }
            for (tab_id, color) in bg_color_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.bg_color = color;
                }
            }
            // Revoke per-tab share tokens on the runtime Tab so the
            // cleared state persists into tabs.json (the snapshot was
            // already cleared by the endpoint for instant 401s).
            for tab_id in token_rotations {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.share_token_rw = "".into();
                    tab.share_token_ro = "".into();
                }
            }
            for (tab_id, context) in context_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.context = context.map(std::sync::Arc::from);
                }
            }
            // Assignment changes — mirrored onto the runtime Tab so the next
            // persist() writes them to tabs.json (they survive a restart).
            for (tab_id, assignment) in assignment_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.assignment = assignment.map(std::sync::Arc::from);
                }
            }
            // Inc8 agent-card mutations — mirrored onto the runtime tab (persisted
            // on the next tick like assignment). See `crate::api::CardChange`.
            for (tab_id, change) in card_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    match change {
                        crate::api::CardChange::Specialty(v) => tab.specialty = v.map(std::sync::Arc::from),
                        crate::api::CardChange::Orchestrator(v) => tab.orchestrator = v.map(std::sync::Arc::from),
                        crate::api::CardChange::Objective(v) => tab.objective = v.map(std::sync::Arc::from),
                        crate::api::CardChange::CurrentTaskAppend(p) => {
                            crate::append_current_task(&mut tab.current_task, &p);
                        }
                        crate::api::CardChange::RoundsActive(ra) => tab.rounds_active = Some(ra),
                        crate::api::CardChange::EvaluationAppend(ev) => {
                            crate::append_evaluation(&mut tab.evaluations, ev);
                        }
                        crate::api::CardChange::Usage(count, stamp) => {
                            tab.usage_count = Some(count);
                            tab.last_used_at = Some(stamp);
                        }
                        crate::api::CardChange::Conventions(list) => tab.conventions = list,
                        crate::api::CardChange::SpawnMode(m) => tab.spawn_mode = m,
                    }
                }
            }
            for (tab_id, parent) in parent_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.parent_tab_id = parent.map(std::sync::Arc::from);
                }
            }
            for (tab_id, rehome) in rehome_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.rehome_status = rehome.map(std::sync::Arc::from);
                }
            }
            // Schedule changes — None clears, Some sets. Mirrors the
            // `locked` / `bg_color` drain above: mutate the runtime
            // `Tab` so the next persist tick rebuilds `tabs.json` +
            // `api_tabs` with the new value.
            for (tab_id, sched) in schedule_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    tab.schedule = sched;
                }
            }
            // Per-tab resource-limit changes (`tab-atelier limit …` / POST
            // /tabs/<id>/limits): `clear` resets every axis, otherwise the
            // override's `Some` axes merge in. Mutating the runtime `Tab`
            // persists the new limits into tabs.json on the next tick; on Linux
            // we also re-apply them live so a running tab is capped (or freed)
            // without a respawn — the same handling the headless daemon does.
            for (tab_id, over, clear) in limit_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == tab_id) {
                    if clear {
                        tab.limits = crate::TabResourceLimits::default();
                    } else {
                        tab.limits.merge(&over);
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let pid = tab.view.read(cx).pid();
                        let effective = crate::TabResourceLimits::resolve(&tab.limits, &self.default_limits);
                        crate::cgroup::reapply(&tab_id, pid, &effective);
                    }
                }
            }
            // Global default-limit change (`tab-atelier limit --all` / POST
            // /limits/default): update the live default, persist it to
            // preferences.json, sync the RAM-gauge mirror, and re-apply the
            // cgroup to every tab so tabs without their own override are
            // recapped now; new tabs read the updated default. cgroups are
            // Linux-only, so this is a no-op (just drained) elsewhere.
            #[cfg(target_os = "linux")]
            if let Some((over, clear)) = default_limit_change {
                if clear {
                    self.default_limits = crate::TabResourceLimits::default();
                } else {
                    self.default_limits.merge(&over);
                }
                self.default_tab_mem_max = self.default_limits.memory_max.clone();
                if !crate::read_only() {
                    let dir = platform::config_dir();
                    let mut prefs = load_preferences(&dir);
                    prefs.default_tab_limits = self.default_limits.clone();
                    save_preferences(&dir, &prefs);
                }
                for tab in &self.tabs {
                    let pid = tab.view.read(cx).pid();
                    let effective = crate::TabResourceLimits::resolve(&tab.limits, &self.default_limits);
                    crate::cgroup::reapply(&tab.id, pid, &effective);
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = default_limit_change; // cgroup limits are Linux-only
            // Per-tick effective-lock mirror.
            //
            // The view's `set_locked()` gate is what stops LOCAL
            // typing in the desktop GUI. We want it driven by the
            // same `effective_locked()` that every API gate uses, so
            // off-hours schedule transitions pause local input the
            // same way a manual lock does — without a dedicated
            // schedule-only push path that future code might miss.
            //
            // Compares against `tab.last_pushed_locked` and skips
            // when unchanged so an idle tab's per-tick recompute is
            // a single bool compare (no gpui notify, no schedule
            // re-eval cost beyond the one in effective_locked()).
            for tab in &mut self.tabs {
                let want = crate::schedule::LockState::effective_locked(tab);
                if tab.last_pushed_locked != Some(want) {
                    tab.view.read(cx).set_locked(want);
                    tab.last_pushed_locked = Some(want);
                }
            }
            for upd in status_updates {
                let Some(tab) = self.tabs.iter_mut().find(|t| *t.id == upd.tab_id) else {
                    continue;
                };
                // "__clear__" sentinel from a POST with state=idle.
                // Wipes BOTH the transient state and the durable
                // session attachment, so the LED actually disappears
                // on Claude Code's SessionEnd hook (otherwise the
                // grey "session attached" dot would stick around).
                if upd.label.as_deref() == Some("__clear__") {
                    tab.agent_state = None;
                    tab.agent_session_id = None;
                    tab.agent_kind = None;
                    tab.agent_plan_mode = None;
                } else {
                    tab.agent_state = Some(crate::AgentStateSnapshot {
                        state: upd.state,
                        label: upd.label,
                        updated_at: std::time::Instant::now(),
                    });
                    if upd.session_id.is_some() {
                        tab.agent_session_id = upd.session_id.map(std::sync::Arc::from);
                    }
                    if upd.agent_kind.is_some() {
                        tab.agent_kind = upd.agent_kind.map(std::sync::Arc::from);
                    }
                    if upd.plan_mode.is_some() {
                        tab.agent_plan_mode = upd.plan_mode;
                    }
                }
            }
            // Working-subprocess sweep: if the agent CLI has a child
            // process alive (Bash tool running `cargo build`, a long
            // `pytest`, …) keep the LED on "thinking" by refreshing
            // the snapshot timestamp. Long-running tool calls would
            // otherwise fall through the 2-min staleness sweep below
            // because no hook fires between `PreToolUse` and
            // `PostToolUse`. Also covers manual subshell commands the
            // user starts inside an active agent tab.
            let now = std::time::Instant::now();
            // Unreviewed-work (blue LED) maintenance. A tab whose agent takes a
            // real turn (Thinking) while you're NOT looking at it is flagged;
            // the flag is sticky (survives the turn ending) so the blue dot
            // means "an agent worked here and you haven't reviewed it." Reviewing
            // — it's the active tab, or someone has its web viewer open — clears
            // it. Gated on Thinking, NOT raw output, so a reboot resuming every
            // agent doesn't blue them all.
            let active = self.active;
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if i == active {
                    // Diagnostic timestamp (shown as "Last seen" in the stats
                    // popup); ages for every non-active tab.
                    tab.last_focused_at = Some(now);
                }
                let is_active = i == active;
                let viewers = tab.view.read(cx).viewer_count();
                let reviewed = is_active || viewers > 0;
                if reviewed {
                    tab.unreviewed_work = false;
                } else if matches!(
                    tab.agent_state.as_ref().map(|s| s.state),
                    Some(crate::AgentState::Thinking)
                ) {
                    // Only a real hook-driven turn (Thinking) marks unreviewed
                    // work — NOT raw PTY output. A claude restart/resume redraws
                    // its ENTIRE TUI, and a build prints: that's output, but not
                    // "work you asked for and must review." Keying off Thinking
                    // stops a reboot (which resumes every agent) from painting
                    // all background tabs blue and forcing a click on each.
                    tab.unreviewed_work = true;
                }
            }
            #[cfg(feature = "catbus")]
            let probe_base = platform::state_base_dir();
            #[cfg(feature = "catbus")]
            let probe_now = std::time::SystemTime::now();
            // (pid, session) of every live agent this tick — persisted as the
            // reaper's provenance record so a crash-leaked ghost can be killed
            // (and only it) on the next startup. See `agent_reaper`.
            #[cfg(feature = "catbus")]
            let mut live_agents: Vec<(u32, String)> = Vec::new();
            // A parked agent (idle at its prompt, printing nothing, not
            // thinking) can't change activity state — skip its subtree
            // walk (and the probe's second walk + sample append) until
            // output resumes, with a 30 s full-sweep beat as failsafe
            // so `Gone` still demotes the LED within half a minute.
            #[cfg(feature = "catbus")]
            let full_sweep = self
                .last_agent_full_sweep
                .get()
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(30));
            #[cfg(feature = "catbus")]
            if full_sweep {
                self.last_agent_full_sweep.set(Some(now));
            }
            #[cfg(feature = "catbus")]
            for tab in &mut self.tabs {
                if tab.agent_kind.is_none() {
                    continue;
                }
                let ring = tab.view.read(cx).ring_len();
                let thinking = tab
                    .agent_state
                    .as_ref()
                    .is_some_and(|s| s.state == crate::AgentState::Thinking);
                if crate::catbus_agent::sweep_may_skip(full_sweep, thinking, ring, tab.sweep_last_ring.get()) {
                    continue;
                }
                tab.sweep_last_ring.set(ring);
                let pid = tab.view.read(cx).pid();
                let (activity, agent_pid) = crate::catbus_agent::agent_activity_with_pid(pid);
                // Cache the found agent pid so the token loop can resolve
                // the session without re-walking the same subtree.
                tab.agent_pid.set(agent_pid);
                // LED transition lives in the shared, tested helper. On
                // `Gone` the durable session anchor (id / kind / plan) is
                // KEPT: the transcript is still on disk and the tab must
                // be able to `--resume` it later. Only Claude Code's
                // explicit SessionEnd (the `__clear__` POST above) drops
                // the durable attachment.
                let alive = crate::catbus_agent::apply_sweep_activity(&mut tab.agent_state, activity, now);
                if alive {
                    live_agents.push((pid, tab.agent_session_id.as_deref().unwrap_or_default().to_string()));
                    // Remember it (start-time-pinned) so close-all / quit can
                    // kill it even if it later escapes its tab's process group.
                    if let Some(st) = crate::agent_reaper::proc_start_time(pid) {
                        self.launched_agents.insert(pid, st);
                    }
                    // Resource sampler: append this tick's CPU/RSS/ctxsw line
                    // (Gone has no process to sample). The PTY child *is*
                    // claude (agent tabs `exec claude`), so `pid` roots the
                    // agent subtree directly.
                    let state = if activity == crate::catbus_agent::AgentActivity::Working {
                        "working"
                    } else {
                        "idle"
                    };
                    self.agent_probe.observe(&probe_base, &tab.name, pid, state, probe_now);
                }
            }
            // Not in read-only: an inspect-only instance must not overwrite
            // the record with processes it didn't launch (it shares the state
            // dir and skips the single-instance lock). Full sweeps only —
            // a partial (parked-tabs-skipped) tick would truncate the
            // record and let a crash-leaked ghost dodge the reaper.
            #[cfg(feature = "catbus")]
            if full_sweep && !crate::read_only() {
                crate::agent_reaper::record_live_agents(&probe_base, &live_agents);
            }
            // Staleness sweep: drop transient LED state when the last
            // update is older than 2 min. Real Claude turns are
            // tool-heavy and the `PreToolUse` hook refreshes the LED
            // on every tool call, so 2 min of total silence is a
            // strong signal the agent is actually idle (or wedged) —
            // we want the LED to demote back to the grey "session
            // attached" dot quickly so the user notices.
            for tab in &mut self.tabs {
                if let Some(snap) = &tab.agent_state
                    && now.duration_since(snap.updated_at).as_secs() > 120
                {
                    tab.agent_state = None;
                }
            }
            // (The process-presence sweep that used to live here re-walked
            // every agent tab's `/proc` subtree a second time per tick; the
            // `AgentActivity::Gone` arm above already demotes the LED from the
            // same walk — and keeps the durable session — so it was pure
            // duplicate syscall traffic.)
            // Auto-resume sweep: type the queued resume command into a tab once
            // its shell is actually up and has printed its prompt — keyed off
            // "the PTY ring has produced bytes", NOT a fixed delay after tab
            // CREATION. Tabs spawn LAZILY (the background loader forks ~2 shells
            // per 40 ms, so a 60-tab restore takes >1 s); a creation-relative
            // timer fired the resume ~500 ms in, while a not-yet-spawned tab had
            // no shell — `flush` then `take()`s the command and sends it into a
            // dead notifier, silently losing it, so `claude` never resumed and
            // the anchor went stale. Gating on real output means each tab
            // resumes whenever its shell comes up, however late. A live shell
            // buffers the typed bytes, so it's safe the moment it's produced its
            // prompt. `flush` takes the command, so each tab fires at most once.
            // Stagger the resumes: a cold start with dozens of restored agent
            // tabs would otherwise type `claude --resume` into every ready shell
            // in ONE tick, launching them all at once — each ~260 MB, all
            // JIT-compiling — which spikes CPU+RAM and freezes the app for
            // seconds. Cap how many fire per persist tick (2 s) so the fleet
            // comes online gradually instead. Each still resumes as soon as its
            // shell is up; only the burst is spread out.
            let mut resumes_left = 4u8;
            for tab in &mut self.tabs {
                if resumes_left == 0 {
                    break;
                }
                if tab.pending_agent_resume.is_some() && tab.view.read(cx).ring_len() > 0 {
                    tab.flush_pending_agent_resume(cx);
                    resumes_left -= 1;
                }
            }
            for (idx, name) in renames {
                self.rename_tab(idx, name);
            }
            closes.sort_unstable();
            closes.dedup();
            for idx in closes.into_iter().rev() {
                if idx < self.tabs.len() && self.tabs.len() > 1 {
                    self.close_tab(idx, cx);
                }
            }
            if let Some(idx) = activate
                && idx < self.tabs.len()
                && self.active != idx
            {
                self.tabs[self.active].deactivate();
                self.active = idx;
                self.tabs[idx].activate();
                self.tabs[idx].flush_pending_restore(cx);
                cx.notify();
            }
            for (idx, bytes) in inputs {
                if idx < self.tabs.len() {
                    self.tabs[idx].view.read(cx).send_input_bytes(bytes);
                }
            }
        }

        if let Some(ref tracker) = self.tracker {
            // Only ping Wakatime when the user has actually touched the
            // active tab in the last 30s. Otherwise the persist tick
            // would flood the API with heartbeats while the terminal
            // sits idle in the system tray.
            let view = self.tabs[self.active].view.read(cx);
            let recently_active = view.last_input_time().is_some_and(|t| t.elapsed().as_secs() < 30);
            if recently_active {
                let cwd = platform::process_cwd(view.pid());
                tracker.record_activity(cwd);
            }
        }

        // Repaint the tab strip so background/remote-driven state actually
        // shows. This 2s tick is the ONLY place the LED fields are refreshed —
        // agent-status hooks (drained above), the blue "unreviewed work" flag,
        // the green→grey streaming demotion, viewer-driven clearing, dead-agent
        // red — but the tab bar only repaints when the App entity is notified,
        // which otherwise happens solely on LOCAL input. Without this, doing
        // work on a tab from the web viewer (or an agent streaming on a
        // background tab) never moves its LED on the desktop until you click.
        // Gated on visibility: a hidden drop-down needn't repaint (reveal drops
        // caches + notifies), keeping idle CPU flat while parked in the tray.
        if self.visible {
            cx.notify();
        }
    }
}
