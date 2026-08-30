// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "gui")]

use crate::api;
use crate::locale::{self, Lang, Strings};
use crate::platform;
#[cfg(feature = "energy")]
use crate::power;
use crate::screenshot;
use crate::terminal::TerminalView;
use crate::theme::{self, CursorStyle, ThemeName};
use crate::tracking::WakatimeTracker;
use crate::{
    DEFAULT_HOTKEYS, FontConfig, Preferences, SavedState, TabState, gpui_key_to_keycode, keycode_label,
    load_preferences, load_state_with_outputs, load_wakatime_key, resolve_font_config, save_preferences, save_state,
    save_tab_output, save_tab_uptime,
};
// Feature-gated extras: clippy --features gui flagged these as
// "unused imports" because the cfg(feature = "energy"/"catbus")
// call sites don't compile in that profile; but the default-features
// build (CI) does need them.
#[cfg(feature = "energy")]
use crate::save_tab_energy;
#[cfg(feature = "catbus")]
use crate::save_tab_tokens;
use crate::{api_url_for_local_clients, restore_resume_command, tab_env_extras};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Application, AsyncApp, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowHandle, WindowOptions, div, px, relative, rgba,
};
use log::{debug, error, info, warn};

// Slice 2B: `AppState::persist` lives in a dedicated child module (pure move).
mod persist;
mod render;

/// Which capture the screenshot menu requested.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScreenshotMode {
    /// The terminal only (tab bar cropped off).
    Tab,
    /// The whole window.
    App,
    /// The whole window, but with every tab name painted over by a solid
    /// redaction bar *before* the frame is captured — so the real names never
    /// reach the image and can't be recovered.
    Redacted,
}

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// How recently a tab must have produced PTY output to read as "talking"
/// (agent actively streaming a reply / spinner / a tool printing) and light
/// the green LED, even when the stored hook-state is `Waiting`/`None` — e.g. a
/// `--resume`d session that continues without a fresh `UserPromptSubmit`. Kept
/// short so the LED reverts to the real state the moment output goes quiet.
use crate::STREAMING_LED_WINDOW;

struct Tab {
    view: Entity<TerminalView>,
    // String-ish fields that flow verbatim into `api::SnapshotTab` are
    // `Arc<str>` so each snapshot rebuild clones a refcount, not bytes.
    name: std::sync::Arc<str>,
    /// Wall-clock instant we started this tab in *this* process run.
    /// Persisted uptime is folded in via `prior_uptime` so a restart
    /// doesn't reset the counter to zero.
    created_at: std::time::Instant,
    /// Uptime accumulated in previous process runs, loaded from
    /// `tab-<name>.uptime.json`. Added to `created_at.elapsed()` in
    /// `Tab::uptime()`.
    prior_uptime: std::time::Duration,
    active_duration: std::time::Duration,
    last_activated: Option<std::time::Instant>,
    /// Unix-millis of the last time the user switched to this tab (activated,
    /// or a viewer newly opened on it). Mirrored into the API snapshot's
    /// `last_used_at` so a remote client can order the list
    /// most-recently-used-first. Written only on the change edge, not per tick.
    last_used_at: Option<u64>,
    /// "Unreviewed work" flag — drives the blue LED. Set true when the agent
    /// works (thinks / streams) on a tab you are NOT currently looking at, and
    /// stays set (sticky) after it stops, so the tab flags "there's output here
    /// you haven't seen." Cleared to false the moment you review the tab (make
    /// it active, or open its web viewer). A tab whose agent never worked never
    /// sets it. Maintained by the LED sweep; read by the tab-strip renderer.
    unreviewed_work: bool,
    /// When this tab was last the foreground (focused) tab — refreshed every
    /// LED sweep for the active tab, ages for the rest. No longer drives the
    /// LED (the `unreviewed_work` flag does that); kept as diagnostic data,
    /// surfaced as "Last seen" in the tab's right-click stats popup.
    last_focused_at: Option<std::time::Instant>,
    /// When this tab last produced terminal output (PTY ring grew). A recent
    /// value means the agent is actively streaming/redrawing — its reply, a
    /// spinner, or a `cargo build` printing — which lights the LED green
    /// ("talking") even without a fresh status hook. `None` = no output yet.
    last_output_at: Option<std::time::Instant>,
    /// Persisted fixed-grid pin (`tab-atelier resize`), mirrored onto the view's
    /// `pinned_grid`. `None` = window-driven sizing. Round-trips through
    /// tabs.json so the size survives a restart.
    pinned_cols: Option<u16>,
    pinned_rows: Option<u16>,
    #[cfg(feature = "energy")]
    energy_wh: f64,
    /// Last `energy_wh` value flushed to disk. Used to skip writes when no
    /// meaningful additional energy has been consumed since last save.
    #[cfg(feature = "energy")]
    energy_wh_last_saved: f64,
    /// Last token usage flushed to `tokens_tab-<name>.json`. Skips the
    /// write when unchanged — `save_tab_tokens` fsyncs the file AND its
    /// directory, and without this gate every agent tab paid those two
    /// fsyncs every 2 s persist tick for an almost-always-identical
    /// ~40-byte file. `Cell` because the token loop borrows `self.tabs`
    /// immutably.
    #[cfg(feature = "catbus")]
    tokens_last_saved: std::cell::Cell<Option<crate::TokenUsage>>,
    /// Ring length at the last token-sidecar probe — see the gate in
    /// `persist`'s token loop.
    #[cfg(feature = "catbus")]
    tokens_last_ring: std::cell::Cell<u64>,
    /// Resident memory (bytes) of the tab's shell subtree, sampled in `persist`
    /// at the 2 s cadence (same walk that fills the API snapshot, #28 S1). Read
    /// by the tab-bar mini gauge (#28 S5). `Cell` because the persist loop
    /// borrows `self.tabs` immutably; `None` until the first sample.
    rss_bytes: std::cell::Cell<Option<u64>>,
    /// Bit pattern of the last `save_tab_uptime` value, to skip
    /// rewriting frozen (deactivated) tabs' files every 30 s.
    uptime_last_saved: std::cell::Cell<Option<u64>>,
    /// Ring length at the last dormant-LED stamp (`last_output_at`) —
    /// tracked separately from `snap_cache`, which only refreshes while
    /// the API has consumers.
    led_last_ring: std::cell::Cell<u64>,
    /// Ring length at the last LED-sweep visit — a parked agent's
    /// subtree walk is skipped until its ring moves (30 s failsafe).
    #[cfg(feature = "catbus")]
    sweep_last_ring: std::cell::Cell<u64>,
    /// Agent CLI pid found by this tick's LED sweep (`None` = no agent /
    /// not yet swept). Lets the token loop resolve the session via
    /// `find_session_for` instead of re-walking the shell's whole /proc
    /// subtree a second time per tick. Transient; a stale pid (agent
    /// restarted) just fails the /proc reads until the next sweep.
    #[cfg(feature = "catbus")]
    agent_pid: std::cell::Cell<Option<u32>>,
    /// Saved scrollback that hasn't been fed back into the terminal yet.
    /// Tabs other than the active one defer this work until first focus
    /// so cold-launch with many tabs doesn't block on vte-parsing each
    /// one's entire history up front.
    pending_restore: Option<String>,
    /// Last cwd we successfully read from /proc/PID/cwd for this tab's
    /// shell. Used as a sticky fallback so that a dead or exited shell
    /// doesn't blank out the persisted cwd on the next tick.
    last_known_cwd: Option<PathBuf>,
    /// String form of `last_known_cwd`. Held alongside the `PathBuf` so the
    /// 2 s persist tick doesn't redo `to_string_lossy` for every tab on
    /// every tick — most ticks see no cwd change at all.
    last_known_cwd_string: Option<std::sync::Arc<str>>,
    /// Stable per-tab UUID — sourced from `TabState.id` on first
    /// load, generated fresh on tab creation. Exported into the
    /// shell as `_TAB_ID` so tools can call `POST /tabs/by-id/{id}/
    /// status` without caring about renames.
    id: std::sync::Arc<str>,
    /// Transient agent status published by a tool inside the tab
    /// (via the local API). Drives the tab-strip LED. Cleared by
    /// the staleness sweep after 5 minutes of no updates.
    agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable: last agent session UUID associated with this tab.
    /// Persisted to tabs.json so auto-resume can pick the same
    /// session back up after a restart.
    agent_session_id: Option<std::sync::Arc<str>>,
    /// Durable: which agent CLI owns the session ("catbus" or
    /// "claude" today). Free-form string so future agents can
    /// register without a code change. Used by the resume path
    /// to decide which command to type.
    agent_kind: Option<std::sync::Arc<str>>,
    /// Durable: whether the agent was in plan / read-only mode
    /// at last save. Restored along with the session so the tab
    /// comes back in the same mode.
    agent_plan_mode: Option<bool>,
    /// Per-tab env vars (`env set --tab <id>`), injected on this tab's spawn.
    /// Mirrors `TabState::tab_env`.
    #[allow(clippy::struct_field_names)] // consistent name across TabState/HeadlessTab
    tab_env: std::collections::BTreeMap<String, String>,
    /// Per-tab share secrets. Minted lazily by the right-click
    /// share-link menu and persisted to tabs.json so URLs survive
    /// restarts. Empty until first share.
    share_token_rw: std::sync::Arc<str>,
    share_token_ro: std::sync::Arc<str>,
    /// Manual lock — user-toggled via right-click / `POST /lock`.
    ///
    /// **Gate authors:** call `tab.effective_locked()` (via
    /// [`crate::schedule::LockState`]) instead of reading this raw
    /// field. The effective state factors in the off-hours
    /// [`Self::schedule`] auto-lock so a new gate can't accidentally
    /// honour only the manual flag.
    locked: bool,
    /// Off-hours auto-lock (Settings → Schedule). When the rule's
    /// current state is closed,
    /// [`crate::schedule::LockState::effective_locked`] reports
    /// `true` even if [`Self::locked`] is false. None ⇒ no schedule,
    /// tab is always-open from the schedule's perspective.
    schedule: Option<crate::schedule::TabSchedule>,
    /// Last value pushed to `view.set_locked()` — the per-tick
    /// mirror in `persist()` compares against this so an idle tab's
    /// effective-lock recompute is a no-op (skip `cx.notify`).
    last_pushed_locked: Option<bool>,
    /// Per-tab background color override (`#RRGGBB`). `None` ⇒ use
    /// the global `Preferences::tab_bg_color`, which itself falls
    /// back to Tomorrow Night Blue.
    bg_color: Option<String>,
    /// Free-text context the in-tab agent set via `set-context` (e.g.
    /// the PR/task it's on). Shown as a hover tooltip on the tab name.
    /// In-memory; set via the API + drained from the snapshot.
    context: Option<std::sync::Arc<str>>,
    /// Stable workflow assignment (`set-assignment`). Persisted (restored from
    /// `TabState`, written back in `persist()`) and hook-immune, unlike `context`.
    assignment: Option<std::sync::Arc<str>>,
    /// Inc8 S1 agent-card fields — persisted + hook-immune, like `assignment`.
    specialty: Option<std::sync::Arc<str>>,
    orchestrator: Option<std::sync::Arc<str>>,
    objective: Option<std::sync::Arc<str>>,
    /// Bounded `current_task` permalog (see [`crate::append_current_task`]).
    current_task: Vec<String>,
    rounds_active: Option<crate::RoundsActive>,
    /// Inc8 S4 — bounded evaluations ring + use counter (`last_used_at` is above).
    evaluations: Vec<crate::Evaluation>,
    usage_count: Option<u64>,
    /// Inc8 fold — declared conventions (`.md` list).
    conventions: Vec<String>,
    /// Inc9 b3 — last context-% reading + unix-millis of the last detected
    /// compaction (brutal drop). Transient (recomputed from the screen each tick).
    last_context_pct: std::cell::Cell<Option<u8>>,
    last_compaction_at: std::cell::Cell<Option<u64>>,
    /// Daemon-liveness probe, cached by the 2 s persist loop (the tab-strip dot
    /// renders every frame and must NOT walk `/proc` there). Raw probe result
    /// (`None` = detection impossible → optimistic; `Some(false)` = down;
    /// `Some(true)` = up); mapped by `daemon_alive_from_probe`. Transient.
    daemon_probe: std::cell::Cell<Option<bool>>,
    /// UUID of the spawning tab (`parent_tab_id`). Persisted like `assignment`.
    parent_tab_id: Option<std::sync::Arc<str>>,
    /// Re-home progress on a predecessor tab. Persisted like `assignment`;
    /// drives the progress badge + gates the "close the predecessor" action.
    rehome_status: Option<std::sync::Arc<str>>,
    /// One-shot resume command queued on tab restore — when the
    /// shell is up the next tick types `<command>\n` into the
    /// PTY, then clears this. Set in `insert_tab` from the
    /// restored `agent_kind` / `agent_session_id` pair.
    pending_agent_resume: Option<String>,
    /// Memoised grid-derived snapshot fields, keyed by the PTY ring's
    /// `total_len`. `persist()` rebuilt the API snapshot for every tab
    /// every 2 s, and the grid scans (`ansi_text_with_cursor(200)` +
    /// 2000-row `raw_screen_text`) dominate that cost. Since all grid
    /// changes arrive as PTY bytes through the ring, an unchanged
    /// `total_len` means the previous scan is still valid. `None` until
    /// the first scan.
    snap_cache: Option<crate::term_export::GridSnapshotCache>,
    /// Per-tab resource-limit overrides (cgroup v2), layered under
    /// `Preferences::default_tab_limits` and applied at spawn on Linux by
    /// both the GUI and the headless daemon. Round-trips through tabs.json
    /// so neither run wipes limits the other set.
    limits: crate::TabResourceLimits,
}

impl crate::schedule::LockState for Tab {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&crate::schedule::TabSchedule> {
        self.schedule.as_ref()
    }
}

impl Tab {
    /// Boot-state constructor: every construction site (restore from
    /// tabs.json, first-run, empty-restore fallback, Cmd-T insert)
    /// funnels here so the boot invariants — grey LED, "just seen"
    /// focus stamp, deferred caches — live in one place. `ts` seeds
    /// the durable fields (a `TabState::default()` for fresh tabs);
    /// `activated` marks the tab foreground from birth.
    fn from_state(
        view: Entity<TerminalView>,
        ts: &TabState,
        cwd: Option<PathBuf>,
        pending_restore: Option<String>,
        pending_agent_resume: Option<String>,
        activated: bool,
    ) -> Self {
        Self {
            view,
            id: ts.id.as_str().into(),
            name: ts.name.as_str().into(),
            created_at: std::time::Instant::now(),
            prior_uptime: std::time::Duration::from_secs_f64(ts.uptime_secs.unwrap_or(0.0)),
            active_duration: std::time::Duration::ZERO,
            last_activated: activated.then(std::time::Instant::now),
            // Inc8 S4: restore the persisted stamp so usage/recency survives a
            // restart; fall back to "now" only for a freshly-activated tab.
            last_used_at: ts.last_used_at.or_else(|| activated.then(crate::unix_millis)),
            // Boots un-flagged (grey): it only goes blue once its
            // agent WORKS while you're not looking. Restoring a tab
            // isn't "new work", so it must not flash blue on restart.
            unreviewed_work: false,
            // "Just seen" at boot so an attached session only ages into
            // the blue dormant state after DORMANT_AFTER_SECS without
            // being opened — not instantly on every restart.
            last_focused_at: Some(std::time::Instant::now()),
            last_output_at: None,
            pinned_cols: ts.pinned_cols,
            pinned_rows: ts.pinned_rows,
            #[cfg(feature = "energy")]
            energy_wh: ts.energy_wh.unwrap_or(0.0),
            #[cfg(feature = "energy")]
            energy_wh_last_saved: ts.energy_wh.unwrap_or(0.0),
            #[cfg(feature = "catbus")]
            tokens_last_saved: std::cell::Cell::new(None),
            #[cfg(feature = "catbus")]
            tokens_last_ring: std::cell::Cell::new(0),
            rss_bytes: std::cell::Cell::new(None),
            uptime_last_saved: std::cell::Cell::new(None),
            led_last_ring: std::cell::Cell::new(0),
            #[cfg(feature = "catbus")]
            sweep_last_ring: std::cell::Cell::new(0),
            #[cfg(feature = "catbus")]
            agent_pid: std::cell::Cell::new(None),
            pending_restore,
            last_known_cwd_string: cwd.as_ref().map(|p| p.to_string_lossy().into()),
            last_known_cwd: cwd,
            agent_state: None,
            agent_session_id: ts.agent_session_id.as_deref().map(std::sync::Arc::from),
            agent_kind: ts.agent_kind.as_deref().map(std::sync::Arc::from),
            agent_plan_mode: ts.agent_plan_mode,
            tab_env: ts.tab_env.clone(),
            share_token_rw: ts.share_token_rw.as_str().into(),
            share_token_ro: ts.share_token_ro.as_str().into(),
            locked: ts.locked,
            schedule: ts.schedule.clone(),
            bg_color: ts.bg_color.clone(),
            context: None,
            // Persisted: restore it so the tab keeps its phase/role across restarts.
            assignment: ts.assignment.as_deref().map(std::sync::Arc::from),
            // Inc8 S1 card fields — restored like `assignment`.
            specialty: ts.specialty.as_deref().map(std::sync::Arc::from),
            orchestrator: ts.orchestrator.as_deref().map(std::sync::Arc::from),
            objective: ts.objective.as_deref().map(std::sync::Arc::from),
            current_task: ts.current_task.clone(),
            rounds_active: ts.rounds_active.clone(),
            // Inc8 S4 — restored like the card fields (usage/recency survive restart).
            evaluations: ts.evaluations.clone(),
            usage_count: ts.usage_count,
            conventions: ts.conventions.clone(),
            last_context_pct: std::cell::Cell::new(None),
            last_compaction_at: std::cell::Cell::new(None),
            daemon_probe: std::cell::Cell::new(None),
            parent_tab_id: ts.parent_tab_id.as_deref().map(std::sync::Arc::from),
            rehome_status: ts.rehome_status.as_deref().map(std::sync::Arc::from),
            last_pushed_locked: None,
            pending_agent_resume,
            snap_cache: None,
            limits: ts.limits.clone(),
        }
    }

    /// Active time this tab has been used (live run + persisted prior runs).
    /// Counts only periods when the user typed in the last 30s — the same
    /// idle threshold `persist()` uses to flip activate/deactivate. Idle
    /// minutes (and time while the drop-down is hidden) don't accumulate,
    /// so a tab left open overnight shows ~the same number in the morning.
    fn uptime(&self) -> std::time::Duration {
        let live = self.last_activated.map(|t| t.elapsed()).unwrap_or_default();
        self.prior_uptime + self.active_duration + live
    }

    fn activate(&mut self) {
        self.last_used_at = Some(crate::unix_millis());
        if self.last_activated.is_none() {
            self.last_activated = Some(std::time::Instant::now());
        }
        // Reviewing a tab clears its "unreviewed work" (blue) flag.
        self.unreviewed_work = false;
    }

    /// If this tab had its scrollback restore deferred until first focus,
    /// feed it through vte now. Cheaper than blocking the cold launch on
    /// every tab's parser pass.
    fn flush_pending_restore(&mut self, cx: &mut gpui::App) {
        if let Some(out) = self.pending_restore.take() {
            self.view.read(cx).restore_output(&out);
            // restore_output feeds the parser directly (not through the
            // PTY ring), so the ring's total_len doesn't move — drop the
            // snapshot cache so the next persist re-scans the restored grid.
            self.snap_cache = None;
        }
    }

    /// Type the queued auto-resume command into the shell, if any.
    /// Fires Ctrl-U first to clear whatever the user may have started
    /// typing, then the command + LF. Same pattern as the "Switch to
    /// catbus" menu item.
    fn flush_pending_agent_resume(&mut self, cx: &mut gpui::App) {
        if let Some(cmd) = self.pending_agent_resume.take() {
            let view = self.view.read(cx);
            view.send_input_bytes(vec![0x15]); // Ctrl-U
            // Clear the grid first so the previous run's tail (e.g. the
            // `claude --resume …` exit line) doesn't linger under the resumed
            // agent's fresh UI — see `crate::AGENT_LAUNCH_CLEAR`.
            let mut bytes = format!("{}{cmd}", crate::AGENT_LAUNCH_CLEAR).into_bytes();
            bytes.push(b'\n');
            view.send_input_bytes(bytes);
        }
    }

    fn deactivate(&mut self) {
        if let Some(t) = self.last_activated.take() {
            self.active_duration += t.elapsed();
        }
    }
}

enum MenuKind {
    Tab(usize),
    Background,
}

struct ContextMenu {
    kind: MenuKind,
    position: Point<Pixels>,
    open_upward: bool,
    /// The detected link under the cursor when the menu opened, if any.
    /// Populated for a terminal-area right-click over a URL/path so the
    /// menu can surface "Copy path (link)"; `None` everywhere else.
    link: Option<String>,
}

struct Toast {
    message: String,
    time: std::time::Instant,
    path: Option<PathBuf>,
}

#[derive(Clone)]
struct DraggedTab {
    idx: usize,
    name: Arc<str>,
    theme: ThemeName,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::theme(self.theme);
        div()
            .px(px(12.0))
            .py(px(4.0))
            .bg(th.elevated_hsla())
            .text_color(th.fg_hsla())
            .text_size(px(13.0))
            .rounded(px(4.0))
            .opacity(0.8)
            .child(self.name.to_string())
    }
}

/// Hover tooltip showing a tab's agent-set context (the PR / task the
/// in-tab agent declared via `tab-atelier set-context "…"`).
struct TabContextTooltip {
    text: String,
    theme: ThemeName,
}

impl Render for TabContextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let th = theme::theme(self.theme);
        div()
            .max_w(px(440.0))
            .px(px(10.0))
            .py(px(6.0))
            .bg(th.elevated_hsla())
            .text_color(th.fg_hsla())
            .text_size(px(12.0))
            .border_1()
            .border_color(th.border_hsla())
            .rounded(px(4.0))
            .child(self.text.clone())
    }
}

struct ExitConfirm {
    tab_idx: usize,
}

/// State for the Ctrl+P MRU tab switcher modal.
struct TabSwitcher {
    /// Tab indices in most-recently-visited order (the current active tab is
    /// excluded — you're already on it), captured when the modal opens.
    order: Vec<usize>,
    /// Highlighted row into `order`. Starts at 0 (the previous tab) so a bare
    /// Ctrl+P → Enter jumps straight back to where you just were.
    selected: usize,
}

/// Everything `render_qr_modal` needs, computed once when the modal opens
/// (see [`AppState::qr_modal`]): interface IPs, the click-to-open URL, and
/// the encoded QR as a dark/light bitmap. Rebuilding the ~2000-div module
/// grid each frame is unavoidable in immediate-mode gpui, but the `ip`
/// subprocess and the QR encode don't have to be.
struct QrModalData {
    ips: Vec<String>,
    url: String,
    qr_width: usize,
    qr_dark: Vec<bool>,
}

/// Height of the tab strip in pixels — matches `render_tab_bar`'s `.h(px(32.0))`.
/// Subtracted from the viewport height to get the terminal area when computing a
/// startup grid size for every tab (so unopened tabs' PTYs are sized right).
const TAB_BAR_HEIGHT: f32 = 32.0;

/// App icon shown on the reusable centered screen (loading / future lock screen).
/// The same 192px raster the web manifest and favicons are generated from.
const LOGO_PNG: &[u8] = include_bytes!("../../assets/icons/icon-192.png");

/// Cols × lines that fit a viewport, given the cell size — the pure arithmetic
/// behind [`AppState::grid_size`]. Subtracts the tab strip from the height.
/// `None` for a not-yet-laid-out (zero) viewport or an unmeasured cell, so the
/// caller keeps the 80×24 spawn fallback rather than a nonsense 2×1 grid.
fn grid_dims(vp_w: f32, vp_h: f32, cell_w: f32, cell_h: f32) -> Option<(usize, usize)> {
    if vp_w < 1.0 || vp_h < 1.0 || cell_w < 1.0 || cell_h < 1.0 {
        return None;
    }
    let cols = ((vp_w / cell_w) as usize).max(2);
    let lines = (((vp_h - TAB_BAR_HEIGHT).max(cell_h) / cell_h) as usize).max(1);
    Some((cols, lines))
}

/// Fill colour for the per-tab RAM gauge, by fill fraction: blue up to 75 %,
/// amber 75–90 %, red past 90 % — so a tab nearing its memory cap (OOM risk)
/// visibly reddens.
fn ram_gauge_fill(frac: f32) -> Rgba {
    if frac >= 0.9 {
        Rgba {
            r: 0.94,
            g: 0.33,
            b: 0.31,
            a: 0.9,
        } // red — at/near the cap
    } else if frac >= 0.75 {
        Rgba {
            r: 0.95,
            g: 0.66,
            b: 0.22,
            a: 0.9,
        } // amber — getting close
    } else {
        Rgba {
            r: 0.36,
            g: 0.60,
            b: 1.0,
            a: 0.85,
        } // blue — comfortable
    }
}

/// One tab's output-save request for the [`OutputSaver`] worker: its name, the
/// ring length (a cheap dirtiness key), and a `Send` closure that serialises the
/// scrollback. The main thread builds these (an `Arc` clone + a brief ring lock
/// per tab); the worker runs the expensive serialize + atomic disk write.
struct SaveJob {
    name: Arc<str>,
    ring_len: u64,
    serialize: Box<dyn FnOnce() -> String + Send>,
}

/// Background thread that runs `copy_all_history` (scrollback → ANSI, up to 10k
/// lines) + the atomic disk write OFF the gpui main thread — the GUI twin of
/// headless's saver. Before this, the 2 s persist tick serialised every changed
/// tab inline on the main thread, so a flood of active tabs stalled typing for
/// up to ~1.5 s (the p99 keystroke spike).
struct OutputSaver {
    tx: std::sync::mpsc::Sender<Vec<SaveJob>>,
}

impl OutputSaver {
    fn spawn(state_base: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<SaveJob>>();
        let spawned = std::thread::Builder::new()
            .name("ta-output-saver".into())
            .spawn(move || {
                // Per-tab dirtiness gate (ring_len, then output crc), kept here in
                // the worker instead of on the `Tab` struct.
                let mut seen: std::collections::HashMap<Arc<str>, (u64, u32)> = std::collections::HashMap::new();
                while let Ok(mut batch) = rx.recv() {
                    // Saves are current-state + idempotent, so if newer batches
                    // queued while we worked, jump to the latest.
                    while let Ok(newer) = rx.try_recv() {
                        batch = newer;
                    }
                    for job in batch {
                        if seen.get(&job.name).is_some_and(|&(rl, _)| rl == job.ring_len) {
                            continue; // ring unchanged ⇒ identical output
                        }
                        let output = (job.serialize)();
                        if output.is_empty() {
                            continue;
                        }
                        let h = crate::crc32(output.as_bytes());
                        if seen.get(&job.name).is_some_and(|&(_, hh)| hh == h) {
                            seen.insert(job.name, (job.ring_len, h));
                            continue;
                        }
                        save_tab_output(&state_base, &job.name, &output);
                        seen.insert(job.name, (job.ring_len, h));
                    }
                }
            });
        // Degrade rather than crash: if the OS won't give us a thread, the app
        // keeps running — tab output just isn't persisted.
        if let Err(e) = spawned {
            warn!("output-saver thread failed to spawn; tab output won't be saved: {e}");
        }
        Self { tx }
    }

    /// Cheap main-thread hand-off (`Arc` clones + a brief ring lock per tab);
    /// never blocks on the scrollback serialize or the disk write.
    fn submit(&self, batch: Vec<SaveJob>) {
        let _ = self.tx.send(batch); // ignore if the saver has exited
    }
}

struct AppState {
    tabs: Vec<Tab>,
    active: usize,
    context_menu: Option<ContextMenu>,
    /// The desktop screen-mate pet — all its state + rendering lives in
    /// [`crate::pet::PetOverlay`]; summoned/dismissed from the background menu.
    #[cfg(feature = "pets")]
    pet: crate::pet::PetOverlay,
    /// When set, tab names render as solid redaction bars instead of text.
    /// Flipped on only for the duration of a "Screenshot (redacted)" capture so
    /// the real names never reach the pixel buffer — nothing to reverse.
    screenshot_censor: bool,
    /// Show the per-tab RAM mini gauge in the tab bar (#28 S5). Mirrors
    /// `Preferences::show_tab_gauge`; toggled from a tab's right-click menu.
    show_tab_gauge: bool,
    /// Force Claude-only mode: new tabs launch `claude` (auto mode) instead of
    /// a shell. Mirrors [`crate::CLAUDE_ONLY`] / `Preferences::claude_only`;
    /// the right-click "New bash tab" item cancels it.
    claude_only: bool,
    /// Relay mode: claude tabs' Anthropic calls go through the configured
    /// remote. Mirrors [`crate::RELAY_MODE`] / `Preferences::relay_mode`;
    /// toggled from the right-click menu.
    relay_mode: bool,
    renaming: Option<(usize, String)>,
    rename_select_all: bool,
    rename_focus: FocusHandle,
    visible: bool,
    /// Lock-free mirror of `visible`, updated wherever `visible` is —
    /// lets the housekeeping loops decide the hidden case (a Guake
    /// terminal's steady state) without entering the entity.
    visible_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    windowed: bool,
    exit_confirm: Option<ExitConfirm>,
    close_confirm: Option<usize>,
    show_qr: bool,
    /// QR-modal data, computed once when the modal opens. The `ip`
    /// subprocess call + Reed-Solomon QR encode used to run inside
    /// `render_qr_modal` on EVERY frame (30-60 fps while the active tab
    /// streams) — a fork+exec per paint. Refreshed on each open so the
    /// IPs still track routing changes (Wi-Fi switch, VPN up/down).
    qr_modal: Option<QrModalData>,
    /// Last title pushed via `set_window_title`, so render only re-sends
    /// it when it actually changes (tab switch / rename), not per frame.
    last_window_title: String,
    /// Cached `"tab-{i}"` element-id strings for the tab bar, grown on
    /// demand — saves a `format!` per tab per frame. Index-keyed, so
    /// entries never need invalidation.
    tab_el_ids: Vec<SharedString>,
    font_config: FontConfig,
    tracker: Option<WakatimeTracker>,
    api_token: String,
    /// `addr:port` bind strings for the three listeners. Sourced from
    /// saved preferences at startup; live changes require a restart
    /// since the `TcpListener`s are bound in spawned threads.
    api_addr: String,
    api_tls_addr: String,
    /// Public base URL for share links (e.g.
    /// `https://example.com/~user/tab-atelier`). Read at "Copy share
    /// link" menu time. Empty → use the LAN URL.
    share_url_base: String,
    /// Global default viewer background color (`#RRGGBB`). `None` →
    /// fall back to the Tomorrow Night Blue default. Per-tab
    /// `Tab::bg_color` wins when set.
    tab_bg_global: Option<String>,
    api_state: Arc<Mutex<api::TabSnapshot>>,
    #[cfg(feature = "energy")]
    power_pids: Arc<Mutex<Vec<u32>>>,
    #[cfg(feature = "energy")]
    power_watts: Arc<Mutex<Vec<power::TabPower>>>,
    #[cfg(feature = "energy")]
    battery_percent: Arc<Mutex<Option<u8>>>,
    /// Owner side of the power sampler's hot/cold switch — persist flips
    /// it from (window visible || API consumer active) so the /proc
    /// sweep slows 5× when nobody can see the numbers.
    #[cfg(feature = "energy")]
    power_hot: Arc<std::sync::atomic::AtomicBool>,
    blink_on: bool,
    toasts: Vec<Toast>,
    lang: Lang,
    theme_name: ThemeName,
    cursor_style: CursorStyle,
    opacity: u8,
    hotkeys: Vec<u8>,
    show_preferences: bool,
    show_hotkey_picker: bool,
    hotkey_picker_focus: FocusHandle,
    hotkey_picker_error: Option<String>,
    /// Ctrl+P MRU tab switcher — `None` when closed. When `Some`, its focus
    /// handle is anchored each render so keys hit the modal, not the terminal.
    tab_switcher: Option<TabSwitcher>,
    tab_switcher_focus: FocusHandle,
    browser: Rc<RefCell<Option<String>>>,
    code_editor: Rc<RefCell<Option<String>>>,
    pref_browser_text: String,
    pref_browser_focus: FocusHandle,
    pref_editor_text: String,
    pref_editor_focus: FocusHandle,
    /// Editable copies of the bind strings shown in the preferences
    /// dialog. Persisted only on Save and applied on next launch (the
    /// API listener threads bind once at startup).
    pref_api_addr_text: String,
    pref_api_addr_focus: FocusHandle,
    pref_api_tls_addr_text: String,
    pref_api_tls_addr_focus: FocusHandle,
    pref_share_url_base_text: String,
    pref_share_url_base_focus: FocusHandle,
    /// Edit buffer + focus for the global "max RAM per tab" default
    /// (`Preferences::default_tab_limits.memory_max`). Empty = unlimited.
    pref_default_mem_text: String,
    pref_default_mem_focus: FocusHandle,
    /// Live mirror of the persisted global per-tab RAM cap
    /// (`default_tab_limits.memory_max`), e.g. `"8G"`. Cross-platform (the
    /// Linux-only `default_limits` field carries the same value for cgroup use);
    /// kept here so the Preferences dialog + save path work on every OS. `None` =
    /// unlimited. Feeds the RAM gauge's denominator via [`Self::tab_mem_ceiling`].
    default_tab_mem_max: Option<String>,
    /// Saved remote `tab-atelier-headless` endpoints. Loaded from
    /// `preferences.json` at startup, edited via the "Remote endpoints"
    /// section of the Preferences modal, and persisted back on Save.
    remote_endpoints: Vec<crate::RemoteEndpoint>,
    /// Global default per-tab cgroup ceilings from
    /// `Preferences::default_tab_limits`, layered under each tab's own
    /// `limits` and applied at every spawn. Linux-only (cgroup v2).
    #[cfg(target_os = "linux")]
    default_limits: crate::TabResourceLimits,
    hotkey_handle: Option<platform::HotkeyHandle>,
    /// When the per-tab uptime files were last written. Persisting uptime
    /// every 2s would burn through disk writes for a value that only
    /// advances by ~2s anyway; we batch writes to once every 30s.
    last_uptime_save: std::cell::Cell<Option<std::time::Instant>>,
    /// Candidate size for `broadcast_active_size`'s two-tick stability
    /// gate — pushed to background tabs only after it stops changing.
    pending_broadcast_size: std::cell::Cell<Option<(usize, usize)>>,
    /// 30 s beat for the complete agent LED sweep; between beats only
    /// non-parked (recently-printing / thinking) agent tabs are walked.
    #[cfg(feature = "catbus")]
    last_agent_full_sweep: std::cell::Cell<Option<std::time::Instant>>,
    /// Persist's fsyncing state writes run here, off the main thread —
    /// see [`crate::StateWriter`]. Shutdown flushes it, then writes
    /// synchronously.
    state_writer: crate::StateWriter,
    /// CRC32 of the last serialized `tabs.json` content. Skips the write+
    /// rotate when nothing in the tab list changed since last tick.
    last_state_hash: std::cell::Cell<u32>,
    /// Per-tab active connection count (metering), keyed by tab id. Refreshed
    /// on a timer from `/proc` (the desktop is unprivileged → connections
    /// only, no nft byte counts). Side map so the `Tab` struct is untouched.
    /// `Arc<Mutex<…>>` (not `RefCell`) so the /proc scan that fills it can
    /// run on the background executor — it stats every process on the host
    /// and readlinks every descendant fd, a 10-50 ms stall when it ran
    /// inline in the 2 s persist tick on the gpui main thread.
    tab_connections: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    /// Last time `tab_connections` was refreshed (throttled — the /proc scan
    /// is too heavy for every persist tick).
    last_conn_meter: std::cell::Cell<Option<std::time::Instant>>,
    /// Last time non-agent tabs were probed for a manually-launched agent
    /// (the token-stats discovery walk) — see `persist`'s token block.
    #[cfg(feature = "catbus")]
    last_token_discovery: std::cell::Cell<Option<std::time::Instant>>,
    /// Mirror of the API snapshot's lock-free `activity` counter (see
    /// `api::TabSnapshot::activity`), so persist-tick work that only serves
    /// API consumers can be skipped entirely while nobody is connected.
    activity_signal: Arc<std::sync::atomic::AtomicU64>,
    activity_last_seen: std::cell::Cell<u64>,
    activity_last_at: std::cell::Cell<Option<std::time::Instant>>,
    /// `render`'s own last-seen activity value (separate from persist's —
    /// they consume the same counter independently). Seeded to `u64::MAX`
    /// so the first frame always checks the pending-new-tab queue.
    render_activity_seen: std::cell::Cell<u64>,
    /// Last `(cols, lines)` broadcast from the active tab to the background
    /// tabs. The active tab computes the real grid size on its first/every
    /// paint; a tick pushes it to the (never-painted) background tabs so their
    /// PTYs + remote viewers match. This skips the O(N) resize loop when the
    /// size is unchanged (the common case — only launch + window resizes move it).
    last_broadcast_size: std::cell::Cell<Option<(usize, usize)>>,
    /// App icon for [`Self::render_center_screen`], wrapped once so the
    /// loading/lock screen doesn't re-wrap the PNG bytes every frame.
    logo: Arc<gpui::Image>,
    /// Worker thread the persist tick hands scrollback-save jobs to, so the
    /// expensive `copy_all_history` + disk write never runs on the gpui main
    /// thread (was the ~1.5 s periodic typing stall under many active tabs).
    output_saver: OutputSaver,
    /// Per-tab agent resource sampler. Every persist tick, each agent
    /// tab's `/proc` subtree is sampled and a JSONL line appended to
    /// `agent_probe_tab-<name>.jsonl` — the "why is idle claude busy"
    /// timeline a future binary taps into. See [`crate::agent_probe`].
    agent_probe: crate::agent_probe::AgentProbe,
    /// Every agent process launched this run, `pid → /proc start_time`.
    /// On close-all / quit we SIGKILL any still alive so a claude that
    /// escaped its tab's process group (respawn race, or one that outlived
    /// its PTY) doesn't leak as a stopped, init-reparented ghost.
    /// Provenance-based: only pids we launched, start-time-pinned so a
    /// reused pid is never hit. In-memory (a crash can't consult it — that
    /// stays the opt-in startup reaper's job).
    launched_agents: std::collections::HashMap<u32, u64>,
}

impl AppState {
    fn t(&self) -> &'static Strings {
        locale::strings(self.lang)
    }

    fn th(&self) -> &'static theme::Theme {
        theme::theme(self.theme_name)
    }

    /// Terminal grid size `(cols, lines, cell)` for the current window, so every
    /// tab's PTY can be spawned at the right size instead of the 80×24 fallback —
    /// a never-opened tab (and its remote xterm.js viewer) is then correctly
    /// sized from birth. `None` before the window has a real size (viewport not
    /// laid out yet); callers fall back to 80×24 and the first paint corrects it.
    /// RAM-gauge denominator for a tab: its effective memory cap (the tab's own
    /// over the global default, Linux cgroup v2) when set, else `sys_ram` (total
    /// system RAM). `None` = no ceiling to scale against → no bar.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))]
    fn tab_mem_ceiling(&self, tab: &Tab, sys_ram: Option<u64>) -> Option<u64> {
        #[cfg(target_os = "linux")]
        let cap = crate::TabResourceLimits::resolve(&tab.limits, &self.default_limits).memory_max_bytes();
        #[cfg(not(target_os = "linux"))]
        let cap = tab.limits.memory_max_bytes();
        cap.or(sys_ram)
    }

    fn grid_size(window: &mut Window, fc: &crate::FontConfig) -> Option<(usize, usize, gpui::Size<Pixels>)> {
        let vp = window.viewport_size();
        let cell = crate::terminal::measure_cell(window, fc);
        let (cols, lines) = grid_dims(
            f32::from(vp.width),
            f32::from(vp.height),
            f32::from(cell.width),
            f32::from(cell.height),
        )?;
        Some((cols, lines, cell))
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename_focus = cx.focus_handle();
        let hotkey_picker_focus = cx.focus_handle();
        let tab_switcher_focus = cx.focus_handle();
        let pref_browser_focus = cx.focus_handle();
        let pref_editor_focus = cx.focus_handle();
        let pref_api_addr_focus = cx.focus_handle();
        let pref_api_tls_addr_focus = cx.focus_handle();
        let pref_share_url_base_focus = cx.focus_handle();
        let pref_default_mem_focus = cx.focus_handle();
        let prefs = load_preferences(&platform::config_dir());
        // Per-tab cgroup ceilings (Linux). Cloned before `prefs` fields
        // are moved below; layered under each tab's own limits at spawn.
        #[cfg(target_os = "linux")]
        let default_limits = prefs.default_tab_limits.clone();
        // Global per-tab RAM cap mirror (cross-platform) — drives the gauge
        // denominator + the Preferences input. Cloned before `prefs` moves.
        let default_tab_mem_max = prefs.default_tab_limits.memory_max.clone();
        // Font: preferences.json `font_family`/`font_size` → zed
        // settings → fontconfig-resolved monospace (the generic
        // "monospace" can render with a too-wide cell advance).
        let font_config = resolve_font_config(&platform::config_dir(), &prefs);
        // Latch the cleared-env opt-in (+ user vars) before any tab
        // spawns below, so every PTY this process creates honours it.
        if prefs.clear_env.unwrap_or(false) {
            crate::CLEAR_ENV.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::set_clear_env_user_vars(prefs.clear_env_vars.clone());
        }
        let browser: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.browser.clone()));
        let code_editor: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(prefs.code_editor.clone()));
        let lang = match prefs.lang.as_deref() {
            Some("fr") => Lang::Fr,
            Some("en") => Lang::En,
            _ => locale::detect_lang(),
        };
        let theme_name = prefs.theme.as_deref().and_then(ThemeName::from_id).unwrap_or_default();
        let cursor_style = prefs
            .cursor_style
            .as_deref()
            .and_then(CursorStyle::from_id)
            .unwrap_or_default();
        // Claude-only mode is on if either the `--claude-only` flag (already in
        // the global) or the persisted preference asked for it; sync the global
        // so both agree from the first tab.
        let claude_only = crate::claude_only() || prefs.claude_only;
        crate::set_claude_only(claude_only);
        // Relay mode: on if the `--relay` flag (already in the global) or the
        // preference asked for it; sync the global. Seed the global tab-env map
        // from the preference so `env set --global` values apply from boot.
        let relay_mode = crate::relay_mode() || prefs.relay_mode;
        crate::set_relay_mode(relay_mode);
        crate::install_relay_config(&prefs);
        crate::set_tab_env_global(prefs.tab_env.clone());
        let opacity = prefs.opacity.unwrap_or(0xb8);
        let hotkeys = if prefs.hotkeys.is_empty() {
            DEFAULT_HOTKEYS.to_vec()
        } else {
            prefs.hotkeys
        };

        // Resolved early so we can export _TAB_ID / TAB_ATELIER_API_URL /
        // TAB_ATELIER_API_TOKEN into each PTY at spawn time. The token
        // file is whatever load_or_generate_token() reads/writes; the
        // API server itself starts later in this same function with the
        // same values.
        let api_token = api::load_or_generate_token();
        let api_addr_resolved = prefs.api_addr.clone().unwrap_or_else(|| crate::DEFAULT_API_ADDR.into());
        let api_url_for_pty = api_url_for_local_clients(&api_addr_resolved);

        // Grid size for the current window, computed once — every tab below
        // spawns its PTY at this size instead of 80×24, so even a tab the user
        // never opens (and its remote viewer) is correctly sized from the start.
        let boot_grid = Self::grid_size(window, &font_config);

        // Delegate our cgroup subtree before any tab spawns, so limits apply
        // from the first shell — and so a runtime `tab-atelier limit …` on a
        // GUI tab can take effect even when nothing is configured at startup.
        // Always attempted (like the headless daemon); a clean no-op when the
        // app's cgroup scope isn't delegated / writable (see cgroup.rs).
        #[cfg(target_os = "linux")]
        crate::cgroup::init(true);

        let (tabs, active, restored_windowed, restored_dashboard_token) = if let Some(mut saved) =
            load_state_with_outputs(&platform::config_base_dir(), &platform::state_base_dir())
        {
            info!("restoring {} tab(s) from saved state", saved.tabs.len());
            let mut tabs = Vec::new();
            let saved_active = saved.active;
            for ts in &mut saved.tabs {
                // The tab that will be shown first forks its shell now (fast
                // first paint + eager scrollback restore). Every other tab is a
                // skeleton — its PTY is forked in the background by the boot
                // loader below, so startup doesn't block on ~60 shell forks.
                // Net-off tabs aren't deferred (they respawn into bubblewrap
                // right after creation, which needs a live process).
                let is_active = tabs.len() == saved_active;
                let defer_spawn = !is_active && !ts.net_disabled;
                let cwd = ts.cwd.as_ref().map(PathBuf::from);
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let colors = ts.colors_enabled;
                let env = tab_env_extras(&ts.id, &api_url_for_pty, &api_token, &ts.tab_env);
                // Launch the agent directly (exec) when we can drive the
                // shell command (cleared-env mode); otherwise fall back to
                // typing the resume in (`pending_agent_resume` below).
                // NEVER in read-only mode: `claude --resume <uuid>` spawns a
                // duplicate agent against a live session, which rotates/strips
                // the session ids in the user's JSON. A read-only instance must
                // stay inert, so it restores tabs as plain shells.
                let agent_launch = if crate::clear_env() && !crate::read_only() {
                    match (&ts.agent_kind, &ts.agent_session_id) {
                        (Some(k), Some(s)) => {
                            // Name the agent process after the tab so `top -H`/`ps`
                            // can tell 20 claudes apart. Only when the launch shell
                            // supports `exec -a`.
                            let title = crate::shell_supports_exec_a(&crate::clear_env_shell_path())
                                .then_some(ts.name.as_str());
                            crate::agent_launch_shell_suffix_instrumented(k, s, ts.agent_plan_mode, title)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let view = cx.new(|cx| {
                    let mut tv = TerminalView::new_with_colors_and_env(
                        cwd.as_deref(),
                        fc,
                        br,
                        ce,
                        colors,
                        env,
                        agent_launch.clone(),
                        boot_grid,
                        defer_spawn,
                        window,
                        cx,
                    );
                    tv.set_theme(theme_name);
                    tv.set_cursor_style(cursor_style);
                    tv
                });
                // Defer restore_output for non-active tabs — feeding the
                // whole scrollback through vte for every tab synchronously
                // is what makes cold launch slow when there's a lot of
                // history. The active tab is restored eagerly so the user
                // sees their last screen the moment the window paints.
                // `take()` instead of clone — with 60 tabs of saved
                // scrollback the clones transiently doubled tens of MB
                // of output strings held by `saved` until scope end.
                let pending_restore = ts.output.take().and_then(|output| {
                    if is_active {
                        debug!("restoring {} chars of output for '{}'", output.len(), ts.name);
                        view.read(cx).restore_output(&output);
                        None
                    } else {
                        Some(output)
                    }
                });
                // Push the persisted effective-lock state onto
                // the view so input is blocked from the moment
                // the tab loads, not just after the first
                // persist tick. Routes through `LockState` so a
                // tab restored OUTSIDE its schedule's open hours
                // also boots locked, not just manually-locked
                // tabs.
                if crate::schedule::LockState::effective_locked(ts) {
                    view.read(cx).set_locked(true);
                }
                // Restore the no-internet sandbox: set the flag and
                // respawn into bubblewrap so the tab comes back
                // airgapped. Skipped (net left on) when bwrap isn't
                // installed, so a persisted net-off tab doesn't boot
                // into a dead shell on a host without bubblewrap.
                // A hot-swap-adopted shell is skipped too: it is still
                // inside the bubblewrap netns the previous run put it
                // in, and respawning would kill exactly the process the
                // handoff kept alive. (Net-off tabs never defer their
                // spawn, so `was_adopted` is already accurate here.)
                if ts.net_disabled && crate::bwrap_available() {
                    view.update(cx, |v, _| {
                        v.set_net_disabled(true);
                        if !v.was_adopted() {
                            v.respawn(cwd.as_deref());
                        }
                    });
                }
                // Auto-resume: if this tab had an agent session and kind
                // persisted, queue the resume command to be typed into the
                // freshly-spawned shell — UNLESS we already launched the
                // agent directly above (then typing it would double-launch).
                // …and never into a hot-swap-adopted shell: its agent is
                // still running, so typing a `--resume` would double-
                // launch the session. `adoptable` covers deferred tabs
                // whose adoption happens later in the boot loader.
                let pending_agent_resume = if agent_launch.is_some()
                    || crate::read_only()
                    || crate::hotswap::adoptable(&ts.id)
                    || view.read(cx).was_adopted()
                {
                    None
                } else {
                    // Shared restore-match (brain/aligator session-less, else
                    // session-carrying) — one source of truth with headless.
                    restore_resume_command(
                        ts.agent_kind.as_deref(),
                        ts.agent_session_id.as_deref(),
                        ts.agent_plan_mode,
                    )
                };
                tabs.push(Tab::from_state(
                    view,
                    ts,
                    cwd,
                    pending_restore,
                    pending_agent_resume,
                    false,
                ));
            }
            if tabs.is_empty() {
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let new_id = crate::default_tab_id();
                let env = tab_env_extras(
                    &new_id,
                    &api_url_for_pty,
                    &api_token,
                    &std::collections::BTreeMap::new(),
                );
                let view = cx.new(|cx| {
                    let mut tv = TerminalView::new_with_colors_and_env(
                        None, fc, br, ce, true, env, None, boot_grid, false, window, cx,
                    );
                    tv.set_theme(theme_name);
                    tv.set_cursor_style(cursor_style);
                    tv
                });
                let seed = TabState {
                    id: new_id,
                    name: locale::strings(lang).terminal.to_owned(),
                    ..TabState::default()
                };
                tabs.push(Tab::from_state(view, &seed, None, None, None, false));
            }
            let active = saved.active.min(tabs.len() - 1);
            tabs[active].activate();
            (tabs, active, saved.windowed, saved.dashboard_share_token)
        } else {
            let fc = font_config.clone();
            let br = browser.clone();
            let ce = code_editor.clone();
            let new_id = crate::default_tab_id();
            let env = tab_env_extras(
                &new_id,
                &api_url_for_pty,
                &api_token,
                &std::collections::BTreeMap::new(),
            );
            let view = cx.new(|cx| {
                let mut tv = TerminalView::new_with_colors_and_env(
                    None, fc, br, ce, true, env, None, boot_grid, false, window, cx,
                );
                tv.set_theme(theme_name);
                tv.set_cursor_style(cursor_style);
                tv
            });
            let seed = TabState {
                id: new_id,
                name: locale::strings(lang).terminal.to_owned(),
                ..TabState::default()
            };
            (
                vec![Tab::from_state(view, &seed, None, None, None, true)],
                0,
                false,
                String::new(),
            )
        };
        if restored_windowed {
            window.toggle_fullscreen();
        }

        // Boot loader: only the active tab forked its shell up front. Warm the
        // rest — skeletons — in the background, a couple per tick, so startup
        // isn't blocked on ~60 shell forks yet restored agents still come back
        // online (their `exec claude` is baked into the deferred spawn). Runs
        // until every tab is spawned, then exits.
        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let done = this
                    .update(cx, |app, cx| {
                        let mut newly: Vec<usize> = Vec::new();
                        for (i, tab) in app.tabs.iter().enumerate() {
                            if newly.len() >= 2 {
                                break;
                            }
                            if !tab.view.read(cx).is_spawned() {
                                tab.view.update(cx, |v, _| v.ensure_spawned());
                                newly.push(i);
                            }
                        }
                        // Apply each newly-spawned tab's per-tab cgroup limits now
                        // that it has a real child pid. The startup pass skipped
                        // deferred tabs (pid 0) so it couldn't move the master into
                        // a tab cgroup (issue #36); this is where they get limited.
                        #[cfg(target_os = "linux")]
                        for &i in &newly {
                            app.apply_tab_limits(i, cx);
                        }
                        app.tabs.iter().all(|t| t.view.read(cx).is_spawned())
                    })
                    .unwrap_or(true);
                if done {
                    // Every tab has spawned (and claimed its hot-swap
                    // handoff, if any). A handoff fd still unclaimed
                    // belongs to a tab that no longer exists — close it
                    // so its orphaned shell gets its HUP instead of
                    // wedging on a full PTY buffer nobody drains.
                    crate::hotswap::close_unclaimed();
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.persist(cx);
                }) else {
                    break;
                };
            }
        })
        .detach();

        // Fast input-drain — the persist tick above runs every 2 s
        // (disk writes, scrollback CRC, …) which means a keystroke
        // POSTed via /input OR pushed via the WS `in` frame can sit
        // in `pending_input` for up to two whole seconds before
        // hitting the PTY. That's the "typing is very slow" report.
        //
        // Separate 16 ms tick that does ONLY the input drain. Other
        // pending queues (lock toggles, schedule changes, status
        // updates, renames, closes) stay on the slow persist path
        // — they're not latency-critical.
        //
        // The tick is signal-driven: producers bump the snapshot's
        // lock-free `activity` counter, and an idle tick is one atomic
        // load on the background executor — no snapshot lock and, more
        // importantly, NO main-thread wake-up. A Guake terminal spends
        // most of its life hidden with no remote connected; the old
        // unconditional loop woke the gpui thread 62×/s forever for a
        // queue that was almost always empty. When the signal has been
        // quiet for a while the poll itself backs off to 250 ms, so a
        // fully idle app costs 4 atomic loads a second. The first
        // remote keystroke after an idle stretch pays ≤250 ms once;
        // everything after runs on the 16 ms tick again. Missed-bump
        // safety net: `persist` drains every pending queue every 2 s.
        cx.spawn(async |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            use std::sync::atomic::Ordering;
            const FAST: std::time::Duration = std::time::Duration::from_millis(16);
            const IDLE: std::time::Duration = std::time::Duration::from_millis(250);
            // How long after the last API/WS activity the fast tick is
            // kept armed (covers think-pauses between keystrokes).
            const HOT: std::time::Duration = std::time::Duration::from_secs(2);
            let Ok(activity) = this.update(cx, |app, _| {
                app.api_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .activity
                    .clone()
            }) else {
                return;
            };
            let mut last_seen = activity.load(Ordering::Relaxed);
            let mut last_change = std::time::Instant::now();
            let mut interval = IDLE;
            loop {
                cx.background_executor().timer(interval).await;
                let seq = activity.load(Ordering::Relaxed);
                if seq == last_seen {
                    // Nothing new — don't touch the main thread, just
                    // decide how soon to look again.
                    if this.upgrade().is_none() {
                        break;
                    }
                    interval = if last_change.elapsed() < HOT { FAST } else { IDLE };
                    continue;
                }
                last_seen = seq;
                last_change = std::time::Instant::now();
                interval = FAST;
                let Ok(()) = this.update(cx, |app, cx| {
                    app.drain_inputs(cx);
                }) else {
                    break;
                };
            }
        })
        .detach();

        // Lock-free mirror of `self.visible` for the housekeeping loops
        // below — the hidden steady state of a Guake terminal is decided
        // off one atomic instead of a main-thread entity wake per tick.
        let visible_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        #[cfg(feature = "energy")]
        let battery_percent_shared: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));

        // Screen-mate pet animation clock: while the pet is on screen AND the
        // drop-down is visible, notify ~20 fps so render() advances the walk.
        // The hidden case (a Guake terminal's steady state) is decided on the
        // lock-free `visible` mirror, so a hidden app doesn't even enter the
        // entity 20×/s — the loop breathes at 500 ms touching one atomic.
        #[cfg(feature = "pets")]
        {
            let visible = visible_flag.clone();
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let shown = visible.load(std::sync::atomic::Ordering::Relaxed);
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(if shown { 50 } else { 500 }))
                        .await;
                    if !shown {
                        if this.upgrade().is_none() {
                            break;
                        }
                        continue;
                    }
                    let Ok(()) = this.update(cx, |app, cx| {
                        if app.visible && app.pet.is_active() {
                            cx.notify();
                        }
                    }) else {
                        break;
                    };
                }
            })
            .detach();
        }

        {
            let visible = visible_flag.clone();
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let shown = visible.load(std::sync::atomic::Ordering::Relaxed);
                    cx.background_executor()
                        .timer(if shown {
                            std::time::Duration::from_millis(500)
                        } else {
                            std::time::Duration::from_secs(1)
                        })
                        .await;
                    // Hidden: the window can't resize and an exit dialog
                    // can't be seen — skip the entity entirely; the first
                    // tick after re-show catches up on both.
                    if !shown {
                        if this.upgrade().is_none() {
                            break;
                        }
                        continue;
                    }
                    let Ok(()) = this.update(cx, |app, cx| {
                        // Keep background tabs sized to the window (the active tab's
                        // real paint size) — cheap no-op unless it changed.
                        app.broadcast_active_size(cx);
                        if app.exit_confirm.is_some() {
                            return;
                        }
                        for (i, tab) in app.tabs.iter().enumerate() {
                            if tab.view.read(cx).has_exited() {
                                app.exit_confirm = Some(ExitConfirm { tab_idx: i });
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
        }

        #[cfg(feature = "energy")]
        {
            let visible = visible_flag.clone();
            let battery = battery_percent_shared.clone();
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    if this.upgrade().is_none() {
                        break;
                    }
                    // The blink only exists to flash the tab bar red on a
                    // critical battery — both "hidden" and "battery fine"
                    // are answered off-thread, so the steady state costs an
                    // atomic load + a mutex peek, not a main-thread wake.
                    let critical = battery
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some_and(|b| b < 10);
                    if !critical || !visible.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let Ok(()) = this.update(cx, |app, cx| {
                        app.blink_on = !app.blink_on;
                        cx.notify();
                    }) else {
                        break;
                    };
                }
            })
            .detach();
        }

        tabs[active].view.read(cx).focus_handle(cx).focus(window);

        // Pick up the api key from Zed's settings when present so the
        // user doesn't need a separate `~/.wakatime.cfg` entry. When
        // absent, wakatime-cli falls back to its own config. Tracking
        // ultimately needs both a key (anywhere) and the cli binary on
        // disk; WakatimeTracker::new returns None if the cli is missing.
        let key = load_wakatime_key(&platform::config_dir());
        let tracker = WakatimeTracker::new(key);
        if tracker.is_some() {
            info!("wakatime tracking enabled");
        }

        // api_token + api_addr were resolved earlier so they could be
        // exported into each PTY's env; reuse them here.
        let api_addr = api_addr_resolved;
        let api_tls_addr = prefs.api_tls_addr.unwrap_or_else(|| crate::DEFAULT_API_TLS_ADDR.into());
        // User-supplied TLS cert + key (Cloudflare Origin etc.). Both
        // paths must be present; a half-configured pair falls back to
        // self-signed with a warning so the operator notices.
        let api_tls_external = match (prefs.api_tls_cert_path.clone(), prefs.api_tls_key_path.clone()) {
            (Some(c), Some(k)) => Some((std::path::PathBuf::from(c), std::path::PathBuf::from(k))),
            (Some(_), None) | (None, Some(_)) => {
                log::warn!("API/TLS: api_tls_cert_path and api_tls_key_path must both be set; using self-signed");
                None
            }
            (None, None) => None,
        };
        let api_tls_client_ca: Option<std::path::PathBuf> =
            prefs.api_tls_client_ca_path.clone().map(std::path::PathBuf::from);
        let share_url_base = prefs.share_url_base.unwrap_or_default();
        let tab_bg_global = prefs.tab_bg_color;
        let remote_endpoints = prefs.remote_endpoints;
        info!("API server starting on {api_addr} (TLS {api_tls_addr})");
        let activity_signal = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let api_state = Arc::new(Mutex::new(api::TabSnapshot {
            tabs: Vec::<api::SnapshotTab>::new(),
            // Set by start_api_server before it serves; an empty master
            // is rejected by the auth gate's non-empty guard, so the brief
            // pre-start window can't authorise anyone.
            master_token: String::new(),
            // Restored from tabs.json so a shared /dashboard link keeps working
            // across restarts; empty until first minted.
            dashboard_share_token: restored_dashboard_token.as_str().into(),
            active: 0,
            #[cfg(feature = "energy")]
            power: Vec::new(),
            #[cfg(feature = "energy")]
            battery_percent: None,
            pending_closes: Vec::new(),
            pending_activate: None,
            pending_input: Vec::new(),
            pending_lock_changes: Vec::new(),
            pending_net_changes: Vec::new(),
            pending_net_allow_changes: Vec::new(),
            pending_ssh_agent_changes: Vec::new(),
            pending_bg_color_changes: Vec::new(),
            pending_context_changes: Vec::new(),
            pending_assignment_changes: Vec::new(),
            pending_parent_changes: Vec::new(),
            pending_rehome_changes: Vec::new(),
            pending_card_changes: Vec::new(),
            pending_token_rotations: Vec::new(),
            pending_schedule_changes: Vec::new(),
            pending_new_tabs: 0,
            pending_new_tab_cwds: std::collections::VecDeque::new(),
            pending_limit_changes: Vec::new(),
            pending_default_limits: None,
            pending_resizes: Vec::new(),
            pending_claude_only: None,
            pending_relay_mode: None,
            pending_env_changes: Vec::new(),
            pending_relay_config: None,
            pending_renames: Vec::new(),
            pending_status_updates: Vec::new(),
            cached_response: None,
            activity: activity_signal.clone(),
            activity_waker: std::sync::Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }));
        let api_read_only = crate::read_only();
        api::start_api_server(api_state.clone(), api_token.clone(), api_read_only, api_addr.clone());
        api::start_api_server_tls(
            api_state.clone(),
            api_token.clone(),
            api_read_only,
            api_tls_addr.clone(),
            api_tls_external,
            api_tls_client_ca,
        );

        #[cfg(feature = "energy")]
        let power_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        #[cfg(feature = "energy")]
        let power_watts: Arc<Mutex<Vec<power::TabPower>>> = Arc::new(Mutex::new(Vec::new()));
        #[cfg(feature = "energy")]
        let battery_percent = battery_percent_shared;
        #[cfg(feature = "energy")]
        let power_hot = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        #[cfg(feature = "energy")]
        power::start_power_monitor(
            power_pids.clone(),
            power_watts.clone(),
            battery_percent.clone(),
            power_hot.clone(),
        );

        // Restore per-tab fixed-size pins (`tab-atelier resize`) onto the views
        // so a pinned tab keeps its size across restarts (and its web viewer
        // isn't oversized) from the first frame.
        for tab in &tabs {
            if let (Some(c), Some(r)) = (tab.pinned_cols, tab.pinned_rows) {
                tab.view
                    .update(cx, |v, _| v.set_pinned_grid(Some((c as usize, r as usize))));
            }
        }

        // Move every spawned tab (restore + the initial tab) into its own
        // per-tab cgroup. No-op unless delegation succeeded above.
        #[cfg(target_os = "linux")]
        for tab in &tabs {
            let pid = tab.view.read(cx).pid();
            crate::cgroup::apply(
                &tab.id,
                pid,
                &crate::TabResourceLimits::resolve(&tab.limits, &default_limits),
            );
        }

        Self {
            tabs,
            active,
            context_menu: None,
            #[cfg(feature = "pets")]
            pet: crate::pet::PetOverlay::default(),
            screenshot_censor: false,
            renaming: None,
            rename_select_all: false,
            rename_focus,
            visible: true,
            visible_flag,
            windowed: restored_windowed,
            exit_confirm: None,
            close_confirm: None,
            show_qr: false,
            qr_modal: None,
            last_window_title: String::new(),
            tab_el_ids: Vec::new(),
            font_config,
            tracker,
            api_token,
            api_addr,
            api_tls_addr,
            share_url_base,
            tab_bg_global,
            api_state,
            #[cfg(feature = "energy")]
            power_pids,
            #[cfg(feature = "energy")]
            power_watts,
            #[cfg(feature = "energy")]
            battery_percent,
            #[cfg(feature = "energy")]
            power_hot,
            blink_on: false,
            toasts: Vec::new(),
            lang,
            theme_name,
            cursor_style,
            opacity,
            show_tab_gauge: prefs.show_tab_gauge,
            claude_only,
            relay_mode,
            hotkeys,
            show_preferences: false,
            show_hotkey_picker: false,
            hotkey_picker_focus,
            hotkey_picker_error: None,
            tab_switcher: None,
            tab_switcher_focus,
            browser,
            code_editor,
            pref_browser_text: String::new(),
            pref_browser_focus,
            pref_editor_text: String::new(),
            pref_editor_focus,
            pref_api_addr_text: String::new(),
            pref_api_addr_focus,
            pref_api_tls_addr_text: String::new(),
            pref_api_tls_addr_focus,
            pref_share_url_base_text: String::new(),
            pref_share_url_base_focus,
            pref_default_mem_text: String::new(),
            pref_default_mem_focus,
            default_tab_mem_max,
            remote_endpoints,
            #[cfg(target_os = "linux")]
            default_limits,
            hotkey_handle: None,
            last_uptime_save: std::cell::Cell::new(None),
            pending_broadcast_size: std::cell::Cell::new(None),
            #[cfg(feature = "catbus")]
            last_agent_full_sweep: std::cell::Cell::new(None),
            state_writer: crate::StateWriter::spawn(),
            last_state_hash: std::cell::Cell::new(0),
            tab_connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
            activity_signal,
            activity_last_seen: std::cell::Cell::new(0),
            activity_last_at: std::cell::Cell::new(None),
            render_activity_seen: std::cell::Cell::new(u64::MAX),
            last_conn_meter: std::cell::Cell::new(None),
            #[cfg(feature = "catbus")]
            last_token_discovery: std::cell::Cell::new(None),
            last_broadcast_size: std::cell::Cell::new(None),
            logo: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, LOGO_PNG.to_vec())),
            output_saver: OutputSaver::spawn(platform::state_base_dir()),
            agent_probe: crate::agent_probe::AgentProbe::default(),
            launched_agents: std::collections::HashMap::new(),
        }
    }

    /// Push the active tab's real (painted) grid size onto every background tab,
    /// so tabs the user hasn't opened — and their remote xterm.js viewers — are
    /// sized to the window instead of stuck at the 80×24 spawn fallback. Cheap:
    /// a no-op until the active tab's measured size actually changes (launch,
    /// window resize), then one `force_resize` per other tab.
    fn broadcast_active_size(&self, cx: &mut Context<Self>) {
        // If the active tab is pinned (`tab-atelier resize`), its size is NOT the
        // window size, so don't broadcast it — background tabs keep their size
        // until a non-pinned tab is active again.
        if self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.view.read(cx).pinned_grid().is_some())
        {
            return;
        }
        let Some((cols, lines, cell)) = self.tabs.get(self.active).and_then(|t| t.view.read(cx).measured_grid()) else {
            return;
        };
        if self.last_broadcast_size.get() == Some((cols, lines)) {
            return;
        }
        // Only push a size the active tab has held for two consecutive
        // 500 ms ticks: a live resize drag otherwise reflowed every
        // background tab's full scrollback (and SIGWINCHed its agent)
        // on each step. Background tabs get the FINAL size once,
        // ~a second after the drag settles.
        if self.pending_broadcast_size.get() != Some((cols, lines)) {
            self.pending_broadcast_size.set(Some((cols, lines)));
            return;
        }
        self.last_broadcast_size.set(Some((cols, lines)));
        for (i, tab) in self.tabs.iter().enumerate() {
            // Pinned tabs (`tab-atelier resize`) keep their fixed size — never
            // reflow them to the window.
            if i == self.active || tab.view.read(cx).pinned_grid().is_some() {
                continue;
            }
            tab.view.update(cx, |v, _| v.force_resize(cols, lines, cell));
        }
    }

    /// A full-window centered screen: app logo, a title, a status subtitle, and
    /// an optional progress bar. The reusable shell behind the boot loading
    /// screen (`progress: Some`) and, later, a lock screen (`progress: None` +
    /// a "Locked" subtitle). Returns `AnyElement` so `render` can early-return
    /// it in place of the normal tab UI.
    fn render_center_screen(
        &self,
        title: impl Into<SharedString>,
        subtitle: impl Into<SharedString>,
        progress: Option<f32>,
    ) -> gpui::AnyElement {
        const BAR_W: f32 = 220.0;
        let t = self.th();
        let column = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(14.0))
            .child(
                gpui::img(gpui::ImageSource::Image(self.logo.clone()))
                    .w(px(96.0))
                    .h(px(96.0)),
            )
            .child(div().text_size(px(22.0)).text_color(t.fg_hsla()).child(title.into()))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(t.fg_muted_hsla())
                    .child(subtitle.into()),
            )
            .when_some(progress, |el, p| {
                el.child(
                    div()
                        .w(px(BAR_W))
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(t.surface_hsla())
                        .child(
                            div()
                                .w(px(BAR_W * p.clamp(0.0, 1.0)))
                                .h(px(4.0))
                                .rounded(px(2.0))
                                .bg(t.accent_hsla()),
                        ),
                )
            });
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.bg_hsla())
            .child(column)
            .into_any_element()
    }

    /// Move tab `idx`'s freshly-spawned shell (and its future children) into a
    /// per-tab cgroup v2 with its effective ceilings (own `limits` over the
    /// global `default_limits`). No-op unless delegation is set up and a limit
    /// is configured (see `cgroup::apply`).
    #[cfg(target_os = "linux")]
    fn apply_tab_limits(&self, idx: usize, cx: &mut Context<Self>) {
        let tab = &self.tabs[idx];
        let pid = tab.view.read(cx).pid();
        crate::cgroup::apply(
            &tab.id,
            pid,
            &crate::TabResourceLimits::resolve(&tab.limits, &self.default_limits),
        );
    }

    /// Flip forced Claude-only mode on/off: update the struct field, the
    /// process-global (read by `insert_tab`), and persist the preference so it
    /// survives a restart. The caller opens tabs / clears the menu as needed.
    fn set_claude_only_mode(&mut self, on: bool) {
        self.claude_only = on;
        crate::set_claude_only(on);
        if !crate::read_only() {
            let mut prefs = load_preferences(&platform::config_dir());
            prefs.claude_only = on;
            save_preferences(&platform::config_dir(), &prefs);
        }
    }

    /// Flip relay mode: struct field + global (read by `tab_env_extras`) +
    /// persisted preference. Applies to tabs spawned after the toggle.
    fn set_relay_mode_mode(&mut self, on: bool) {
        self.relay_mode = on;
        crate::set_relay_mode(on);
        if !crate::read_only() {
            let mut prefs = load_preferences(&platform::config_dir());
            prefs.relay_mode = on;
            save_preferences(&platform::config_dir(), &prefs);
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.tabs.len(), None, window, cx);
    }

    /// Like `add_tab` but with an explicit cwd hint from the API
    /// (`POST /tabs` with `{cwd: ...}`). Falls back to the existing
    /// inherit-from-active behaviour when the path doesn't exist.
    fn add_tab_in(&mut self, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.tabs.len(), Some(cwd), window, cx);
    }

    fn add_tab_after_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_tab(self.active + 1, None, window, cx);
    }

    fn insert_tab(&mut self, at: usize, hint: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = hint.filter(|p| p.is_dir()).or_else(|| {
            let pid = self.tabs[self.active].view.read(cx).pid();
            platform::process_cwd(pid).or_else(|| self.tabs[self.active].last_known_cwd.clone())
        });
        let grid = Self::grid_size(window, &self.font_config);
        self.tabs[self.active].deactivate();
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let tn = self.theme_name;
        let cs = self.cursor_style;
        let new_id = crate::default_tab_id();
        let env = tab_env_extras(
            &new_id,
            &api_url_for_local_clients(&self.api_addr),
            &self.api_token,
            &std::collections::BTreeMap::new(),
        );
        // Claude-only mode: a fresh tab launches `claude` in `auto` permission
        // mode instead of a plain shell. Under cleared-env we can `exec`
        // it directly via the shell suffix; otherwise we type the command into
        // the shell once its prompt appears (`pending_agent_resume`, below) —
        // the same two mechanisms the restore path uses for agents. Read-only
        // never force-launches. See `crate::FRESH_CLAUDE_AUTO_CMD`.
        let force_claude = self.claude_only && !crate::read_only();
        let exec_claude = force_claude && crate::clear_env();
        let agent_launch = exec_claude.then(crate::fresh_claude_launch_suffix);
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new_with_colors_and_env(
                cwd.as_deref(),
                fc,
                br,
                ce,
                true,
                env,
                agent_launch,
                grid,
                false,
                window,
                cx,
            );
            tv.set_theme(tn);
            tv.set_cursor_style(cs);
            tv
        });
        let idx = at.min(self.tabs.len());
        // Non-exec claude launch: queue the command to be typed into the shell
        // once it prints its first prompt (`flush_pending_agent_resume`).
        let pending_claude = (force_claude && !exec_claude).then(|| crate::FRESH_CLAUDE_AUTO_CMD.to_string());
        let name = if force_claude {
            format!("claude {}", self.tabs.len())
        } else {
            format!("{} {}", self.t().terminal_n, self.tabs.len())
        };
        let seed = TabState {
            id: new_id,
            name,
            ..TabState::default()
        };
        self.tabs
            .insert(idx, Tab::from_state(view, &seed, cwd, None, pending_claude, true));
        self.active = idx;
        #[cfg(target_os = "linux")]
        self.apply_tab_limits(idx, cx);
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn move_tab(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        let new_to = if from < to { to - 1 } else { to };
        self.tabs.insert(new_to, tab);
        self.active = if self.active == from {
            new_to
        } else {
            let mut a = self.active;
            if from < a {
                a -= 1;
            }
            if new_to <= a {
                a += 1;
            }
            a
        };
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let was_active = self.active == idx;
        self.tabs[idx].deactivate();
        let pid = self.tabs[idx].view.read(cx).pid();
        self.tabs[idx].view.read(cx).shutdown();
        // Hard-kill the tab's process group — shutdown() only drops the PTY
        // (SIGHUP), which `claude` can survive and orphan (the ghost sessions).
        #[cfg(unix)]
        crate::kill_tab_pgroup(pid);
        self.agent_probe.forget(&self.tabs[idx].name);
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > idx {
            self.active -= 1;
        }
        if was_active {
            self.tabs[self.active].activate();
            self.tabs[self.active].flush_pending_restore(cx);
        }
        self.context_menu = None;
        cx.notify();
    }

    /// Drain `pending_input` from the API snapshot and ship the bytes
    /// to each tab's PTY. Called on a fast 50 ms tick by the spawn in
    /// `init`, so WS / HTTP keystrokes don't wait up to 2 s for the
    /// next `persist` tick. The slow persist still drains every
    /// pending queue (a no-op for input once we've cleared it here).
    fn drain_inputs(&mut self, cx: &mut Context<Self>) {
        let inputs: Vec<(usize, Vec<u8>)> = {
            let mut snapshot = self.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if snapshot.pending_input.is_empty() {
                return;
            }
            snapshot.pending_input.drain(..).collect()
        };
        for (idx, bytes) in inputs {
            if idx < self.tabs.len() {
                self.tabs[idx].view.read(cx).send_input_bytes(bytes);
            }
        }
    }

    fn respawn_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = platform::process_cwd(old_pid).or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.read(cx).shutdown();
        // Kill the old process group before respawning — otherwise a claude
        // that survived the PTY-close SIGHUP orphans and the fresh spawn's
        // `--resume` loads a duplicate of the same session.
        #[cfg(unix)]
        crate::kill_tab_pgroup(old_pid);
        let grid = Self::grid_size(window, &self.font_config);
        let fc = self.font_config.clone();
        let br = self.browser.clone();
        let ce = self.code_editor.clone();
        let tn = self.theme_name;
        let cs = self.cursor_style;
        let env = tab_env_extras(
            &self.tabs[idx].id,
            &api_url_for_local_clients(&self.api_addr),
            &self.api_token,
            &self.tabs[idx].tab_env,
        );
        // Respawning an agent tab → relaunch the agent directly (exec), same as
        // a restore, so it comes back as claude rather than a bare shell. Never
        // in read-only mode — see the restore path: resuming a live session
        // corrupts the user's session ids.
        let agent_launch = if crate::clear_env() && !crate::read_only() {
            match (&self.tabs[idx].agent_kind, &self.tabs[idx].agent_session_id) {
                (Some(k), Some(s)) => {
                    // Name the agent process after the tab (see the restore path).
                    let title =
                        crate::shell_supports_exec_a(&crate::clear_env_shell_path()).then_some(&*self.tabs[idx].name);
                    crate::agent_launch_shell_suffix_instrumented(k, s, self.tabs[idx].agent_plan_mode, title)
                }
                _ => None,
            }
        } else {
            None
        };
        let view = cx.new(|cx| {
            let mut tv = TerminalView::new_with_colors_and_env(
                cwd.as_deref(),
                fc,
                br,
                ce,
                true,
                env,
                agent_launch,
                grid,
                false,
                window,
                cx,
            );
            tv.set_theme(tn);
            tv.set_cursor_style(cs);
            tv
        });
        self.tabs[idx].view = view;
        #[cfg(target_os = "linux")]
        self.apply_tab_limits(idx, cx);
        self.tabs[idx].created_at = std::time::Instant::now();
        self.tabs[idx].prior_uptime = std::time::Duration::ZERO;
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        #[cfg(feature = "energy")]
        {
            self.tabs[idx].energy_wh = 0.0;
        }
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Number of agent tabs whose durable session survived but whose process is
    /// gone (the dim-red "dead" LED). Drives the "Relaunch dead agents" menu
    /// item's visibility + count. Always 0 without the catbus sweep (no liveness).
    #[cfg_attr(not(feature = "catbus"), allow(clippy::unused_self, clippy::missing_const_for_fn))]
    fn dead_agent_count(&self) -> usize {
        #[cfg(feature = "catbus")]
        {
            self.tabs
                .iter()
                .filter(|t| {
                    t.agent_kind.is_some()
                        && t.agent_kind.as_deref() != Some("brain")
                        && t.agent_session_id.is_some()
                        && t.agent_pid.get().is_none()
                })
                .count()
        }
        #[cfg(not(feature = "catbus"))]
        {
            0
        }
    }

    /// Relaunch every agent tab whose process died (failed auto-resume, crash,
    /// or kill) — `respawn_tab` rebuilds the `--resume <id>` launch, so each
    /// comes back on its persisted session. In non-clear-env mode respawn yields
    /// a bare shell, so also queue the typed resume there. No-op in read-only.
    #[cfg_attr(not(feature = "catbus"), allow(clippy::unused_self, clippy::missing_const_for_fn))]
    fn relaunch_dead_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "catbus")]
        {
            if crate::read_only() {
                return;
            }
            let dead: Vec<usize> = self
                .tabs
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    t.agent_kind.is_some()
                        && t.agent_kind.as_deref() != Some("brain")
                        && t.agent_session_id.is_some()
                        && t.agent_pid.get().is_none()
                })
                .map(|(i, _)| i)
                .collect();
            for idx in dead {
                self.respawn_tab(idx, window, cx);
                // clear-env respawn already exec's the agent; otherwise queue the
                // typed `--resume` so the agent comes back in the fresh shell too.
                if !crate::clear_env() {
                    let kind = self.tabs[idx].agent_kind.as_deref().map(str::to_string);
                    let sid = self.tabs[idx].agent_session_id.as_deref().map(str::to_string);
                    let plan = self.tabs[idx].agent_plan_mode;
                    if let (Some(k), Some(s)) = (kind, sid) {
                        self.tabs[idx].pending_agent_resume = crate::build_agent_resume_command(&k, &s, plan);
                    }
                }
            }
        }
        #[cfg(not(feature = "catbus"))]
        {
            let _ = (window, cx);
        }
    }

    fn respawn_tab_with_history(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_pid = self.tabs[idx].view.read(cx).pid();
        let cwd = platform::process_cwd(old_pid).or_else(|| Some(std::env::current_dir().unwrap_or_default()));
        self.tabs[idx].view.update(cx, |view, _| {
            view.respawn(cwd.as_deref());
        });
        #[cfg(target_os = "linux")]
        self.apply_tab_limits(idx, cx);
        self.tabs[idx].created_at = std::time::Instant::now();
        self.tabs[idx].prior_uptime = std::time::Duration::ZERO;
        self.tabs[idx].active_duration = std::time::Duration::ZERO;
        self.tabs[idx].last_activated = if idx == self.active {
            Some(std::time::Instant::now())
        } else {
            None
        };
        #[cfg(feature = "energy")]
        {
            self.tabs[idx].energy_wh = 0.0;
        }
        self.exit_confirm = None;
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Rename a tab in place, moving the per-tab output/uptime/power files
    /// across so history sticks to the tab through the rename. No-op for
    /// out-of-range index or when the name doesn't change.
    fn rename_tab(&mut self, idx: usize, new_name: String) {
        if idx >= self.tabs.len() {
            return;
        }
        let old_name = self.tabs[idx].name.clone();
        if *old_name == *new_name {
            return;
        }
        if !crate::read_only() {
            let base = platform::state_base_dir();
            for resolver in [
                crate::tab_output_path as fn(&std::path::Path, &str) -> std::path::PathBuf,
                crate::tab_uptime_path,
                crate::tab_power_path,
            ] {
                let old_path = resolver(&base, &old_name);
                let new_path = resolver(&base, &new_name);
                if old_path.exists() {
                    let _ = std::fs::rename(&old_path, &new_path);
                    let _ = std::fs::rename(old_path.with_extension("json.bak"), new_path.with_extension("json.bak"));
                }
            }
        }
        self.tabs[idx].name = new_name.into();
    }

    /// Unconditional flush of tabs.json + every tab's output / uptime /
    /// energy files — the "this process is about to go away" save.
    /// Shared by `close_all_tabs` (quit) and `hot_swap` (exec into the
    /// next binary).
    fn flush_all_state(&mut self, cx: &mut Context<Self>) {
        let state_base = platform::state_base_dir();
        // Snapshot cwd from /proc one last time before child processes
        // disappear; fall back to the cached last_known_cwd otherwise.
        for tab in &mut self.tabs {
            let pid = tab.view.read(cx).pid();
            if let Some(p) = platform::process_cwd(pid)
                && tab.last_known_cwd.as_deref() != Some(p.as_path())
            {
                tab.last_known_cwd_string = Some(p.to_string_lossy().into());
                tab.last_known_cwd = Some(p);
            }
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
                    agent_plan_mode: tab.agent_plan_mode,
                    tab_env: tab.tab_env.clone(),
                    pinned_cols: tab.pinned_cols,
                    pinned_rows: tab.pinned_rows,
                    share_token_rw: tab.share_token_rw.to_string(),
                    share_token_ro: tab.share_token_ro.to_string(),
                    locked: tab.locked,
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
        if !crate::read_only() {
            // Drain queued periodic writes FIRST so none of them can land
            // after (and clobber) the final synchronous state below.
            self.state_writer.flush();
            let dashboard_share_token = self
                .api_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .dashboard_share_token
                .to_string();
            save_state(
                &platform::config_base_dir(),
                &SavedState {
                    tabs,
                    active: self.active,
                    windowed: self.windowed,
                    dashboard_share_token,
                },
            );
            for tab in &self.tabs {
                let output = tab.view.read(cx).copy_all_history();
                if !output.is_empty() {
                    save_tab_output(&state_base, &tab.name, &output);
                }
            }
            // Always flush uptime + energy on shutdown — bypass throttles so
            // the last tick isn't lost.
            for tab in &self.tabs {
                save_tab_uptime(&state_base, &tab.name, tab.uptime().as_secs_f64());
            }
            #[cfg(feature = "energy")]
            for tab in &mut self.tabs {
                save_tab_energy(&state_base, &tab.name, tab.energy_wh);
                tab.energy_wh_last_saved = tab.energy_wh;
            }
        }
    }

    /// Replace this process with the binary at our own install path,
    /// handing every live tab's PTY across the exec (see
    /// [`crate::hotswap`]). The window closes and reopens; the shells —
    /// and whatever is running in them — never notice. Returns only if
    /// the exec failed, in which case we keep running as before.
    #[cfg(unix)]
    fn hot_swap(&mut self, cx: &mut Context<Self>) {
        crate::hotswap::clear_upgrade_request();
        self.flush_all_state(cx);
        let mut sources = Vec::new();
        for tab in &self.tabs {
            let view = tab.view.read(cx);
            // Skeletons (no shell yet) and exited shells carry no fd —
            // they restore from tabs.json exactly like today.
            if view.has_exited() {
                continue;
            }
            let Some(master) = view.handoff_master() else {
                continue;
            };
            let ring_arc = view.pty_ring();
            let ring = ring_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .since(0);
            sources.push(crate::hotswap::HandoffSource {
                id: tab.id.to_string(),
                master,
                pid: view.pid(),
                ring,
            });
        }
        log::info!("hot swap: handing off {} live tab(s)", sources.len());
        let err = crate::hotswap::exec_swap(&sources);
        log::error!("hot swap failed, continuing on the old binary: {err}");
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        self.flush_all_state(cx);

        if let Some(ref tracker) = self.tracker {
            tracker.shutdown();
        }
        for tab in &self.tabs {
            let pid = tab.view.read(cx).pid();
            tab.view.read(cx).shutdown();
            // Kill each tab's process group on app quit — a bare SIGHUP lets
            // claude survive and orphan, and the next launch resumes duplicates.
            #[cfg(unix)]
            crate::kill_tab_pgroup(pid);
        }
        // Provenance sweep: also kill any agent we launched this run that
        // escaped its tab's process group (respawn race, or a claude that
        // outlived its PTY) — otherwise it leaks as a stopped, init-
        // reparented ghost that no later "close all" can reach. Start-time-
        // pinned so a reused pid is never hit.
        #[cfg(unix)]
        for (&pid, &start) in &self.launched_agents {
            if crate::agent_reaper::proc_start_time(pid) == Some(start) {
                crate::kill_tab_pgroup(pid);
            }
        }
        cx.quit();
    }

    fn do_screenshot(&mut self, mode: ScreenshotMode, cx: &mut Context<Self>) {
        // Redacted shots must not leak the name via the filename either.
        let tab_name = if mode == ScreenshotMode::Redacted {
            "redacted".to_string()
        } else {
            self.tabs[self.active].name.to_string()
        };
        // Turn tab names into redaction bars for this capture. The frame renders
        // censored (below, via `cx.notify()`), we wait, capture, then clear it.
        if mode == ScreenshotMode::Redacted {
            self.screenshot_censor = true;
        }
        let progress_time = std::time::Instant::now();
        self.toasts.push(Toast {
            message: self.t().taking_screenshot.into(),
            time: progress_time,
            path: None,
        });
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
            let _ = this.update(cx, |state, cx| {
                state.toasts.retain(|t| t.time != progress_time);
                cx.notify();
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    match mode {
                        ScreenshotMode::Tab => screenshot::take_screenshot_tab(&tab_name, 32),
                        ScreenshotMode::App | ScreenshotMode::Redacted => screenshot::take_screenshot_full(&tab_name),
                    }
                })
                .await;
            let toast_time = std::time::Instant::now();
            let _ = this.update(cx, |state, cx| {
                // Names back to normal now that the frame's been captured.
                state.screenshot_censor = false;
                let t = state.t();
                let (msg, path) = match result {
                    Ok(path) => (t.saved.to_string(), Some(path)),
                    Err(e) => (format!("{}: {e}", t.screenshot_failed), None),
                };
                state.toasts.push(Toast {
                    message: msg,
                    time: toast_time,
                    path,
                });
                cx.notify();
            });
            cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
            let _ = this.update(cx, |state, cx| {
                state.toasts.retain(|t| t.time != toast_time);
                cx.notify();
            });
        })
        .detach();
    }


    /// Summon one more random pet onto the screen (repeated calls grow the herd).
    /// Loads a baked sprite sheet + animation XML from `/usr/share/tab-atelier/pets/`
    /// (dev: `./assets/pets/`). No-op if no pets are installed.
    #[cfg(feature = "pets")]
    fn summon_pet(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        self.pet.summon(f32::from(vp.width), f32::from(vp.height));
    }



    /// Switch to tab `idx`: deactivate the current tab, activate the target,
    /// focus its terminal, and flush any deferred scrollback restore. Shared by
    /// Alt+Tab and the Ctrl+P switcher.
    fn select_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        // Stamp the tab we're leaving so its "… ago" in the switcher is exact,
        // not up to one persist tick stale.
        self.tabs[self.active].last_focused_at = Some(std::time::Instant::now());
        if idx != self.active {
            self.tabs[self.active].deactivate();
            self.active = idx;
            self.tabs[self.active].activate();
        }
        self.tabs[self.active].flush_pending_restore(cx);
        // The tab we're switching TO wasn't mounted while backgrounded, so its
        // grid may have advanced (remote/web-driven output, a TUI redraw) with
        // no paint — its render caches are stale. Drop them + notify so the
        // first frame rebuilds from the live grid; otherwise the desktop shows
        // the frame from when we last left the tab (the "output not up to date
        // after doing stuff from the web" bug).
        self.tabs[self.active].view.update(cx, |v, vcx| {
            v.release_render_caches();
            vcx.notify();
        });
        self.tabs[self.active].view.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }






    /// Gather the QR modal's inputs: interface IPs (one `ip` subprocess
    /// call), the deep-link QR bitmap, and the clickable URL. Called once
    /// per modal open — refreshed each time so the IPs reflect the current
    /// routing table (Wi-Fi switch, VPN up/down, …), but never per frame.
    fn build_qr_modal_data(&self) -> Option<QrModalData> {
        let ips = api::local_ips_all();
        let primary_ip = ips.first().cloned().unwrap_or_else(|| "127.0.0.1".into());
        let lan_url = format!(
            "http://{primary_ip}:{}",
            port_of(&self.api_addr, crate::DEFAULT_API_PORT)
        );
        let lan_url_tls = format!(
            "https://{primary_ip}:{}",
            port_of(&self.api_tls_addr, crate::DEFAULT_API_PORT + 1)
        );
        // Pass both the plain-HTTP and TLS URLs into the deep link; the
        // mobile client picks whichever its current build supports.
        let qr_payload = format!(
            "taremote://onboard?url={lan_url}&tls_url={lan_url_tls}&token={}",
            self.api_token
        );
        let url = format!("{lan_url}?token={}", self.api_token);
        let qr = qrcode::QrCode::new(qr_payload.as_bytes()).ok()?;
        let qr_width = qr.width();
        let qr_dark = qr.to_colors().iter().map(|c| *c == qrcode::Color::Dark).collect();
        Some(QrModalData {
            ips,
            url,
            qr_width,
            qr_dark,
        })
    }



}


// Shared with the headless binary — see `crate::tab_env_extras`,
// `crate::api_url_for_local_clients`, and
// `crate::build_agent_resume_command` in lib.rs.

/// Pull the port out of an `addr:port` bind string. Falls back to
/// `fallback` when the string is malformed (covers IPv4, IPv6 like
/// `[::1]:N`, and bare `:N`).
fn port_of(bind: &str, fallback: u16) -> u16 {
    bind.rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(fallback)
}

/// `addr:port` is a small, well-bounded ASCII subset (digits, dots,
/// colons, brackets, hex letters for IPv6). Anything else is junk and
/// we refuse to insert it so the `SocketAddr` parse on Save can't fail
/// in subtle ways.
fn is_addr_port_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '.' | ':' | '[' | ']') || ('a'..='f').contains(&c.to_ascii_lowercase())
}

/// Char predicate for the "max RAM per tab" input: a digit or a single K/M/G/T
/// (1024-based) suffix. Matches what [`crate::TabResourceLimits::memory_max_bytes`]
/// accepts, so anything typed either parses or is a partial prefix of it.
const fn is_mem_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c.to_ascii_uppercase(), 'K' | 'M' | 'G' | 'T')
}

const MAX_ADDR_LEN: usize = 64;

/// Char predicate for the share-URL-base input — accepts the URL-safe
/// ASCII set (RFC 3986 reserved + unreserved + a few practical extras
/// like spaces / `?` not really allowed but tolerated for paste).
const fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            ':' | '/'
                | '.'
                | '-'
                | '_'
                | '~'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

const MAX_URL_LEN: usize = 256;

/// Tab indices for the Ctrl+P switcher: every index except `active`, ordered
/// most-recently-focused first. `None` (never focused) sorts last; ties keep
/// their original relative order (stable sort). Pure so it can be unit-tested
/// without a live window.
fn mru_tab_order<T: Ord>(active: usize, last_focused: &[Option<T>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..last_focused.len()).filter(|&i| i != active).collect();
    order.sort_by(|&a, &b| last_focused[b].cmp(&last_focused[a]));
    order
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

fn run_check() {
    println!("tab-atelier v{} --check", env!("CARGO_PKG_VERSION"));

    let libs: &[(&str, &str)] = &[
        ("libfreetype.so.6", "libfreetype6"),
        ("libxkbcommon.so.0", "libxkbcommon0"),
        ("libxkbcommon-x11.so.0", "libxkbcommon-x11-0"),
        ("libxcb.so.1", "libxcb1"),
        ("libxcb-xkb.so.1", "libxcb-xkb1"),
    ];
    let mut ok = true;
    let mut missing = Vec::new();
    for (lib, pkg) in libs {
        print!("  {lib:<30}");
        let found = std::path::Path::new("/usr/lib/x86_64-linux-gnu").join(lib).exists()
            || std::path::Path::new("/usr/lib64").join(lib).exists()
            || std::path::Path::new("/usr/lib").join(lib).exists();
        if found {
            println!("ok");
        } else {
            println!("MISSING  (apt install {pkg})");
            missing.push(*pkg);
            ok = false;
        }
    }

    print!("  /dev/ptmx (pty support) ..... ");
    if std::path::Path::new("/dev/ptmx").exists() {
        println!("ok");
    } else {
        println!("MISSING");
        ok = false;
    }

    let state_dir = platform::state_base_dir();
    print!("  state dir ................... ");
    println!("{}", state_dir.display());

    let config_dir = platform::config_dir();
    print!("  config dir .................. ");
    println!("{}", config_dir.display());

    if ok {
        println!("all checks passed");
    } else {
        println!("\nTo fix, run:\n  sudo apt install {}", missing.join(" "));
        std::process::exit(1);
    }
}

/// Launch the gpui application. Blocks until the window closes.
///
/// # Panics
/// Panics if gpui fails to open its initial window (e.g. no X server).
pub fn run() {
    // Single logger init for the GUI. Routes to <state>/tab-atelier.log
    // when a filter is set (`tab-atelier log …` / TAB_ATELIER_LOG /
    // RUST_LOG), else installs nothing — the desktop has no terminal, so
    // stderr logging is pointless. Must be the ONLY init: it uses
    // try_init, so a second env_logger::init() here would panic once a
    // file logger is installed.
    crate::init_gui_file_logging();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        run_check();
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tab-atelier v{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    info!("starting Tab Atelier v{}", env!("CARGO_PKG_VERSION"));

    // Hot-swap handoff: when the previous binary exec'd into us it left
    // `--handoff <manifest>` on argv, naming the live PTY fds it kept
    // open across the exec. Adopt them BEFORE the reaper and the tab
    // restore below, so restored tabs reattach to their running shells
    // instead of forking fresh ones.
    let adopted = crate::hotswap::adopt_from_args();
    if adopted > 0 {
        info!("hot swap: inherited {adopted} live tab(s) from the previous binary");
    }

    // Reap agent processes leaked by a prior (unclean) run before we
    // restore any tab — reclaims the stopped `claude` ghosts that
    // reparented to init. Provenance-based (only kills processes this GUI
    // recorded launching, identity-pinned by start-time), so it can never
    // touch a `claude` running elsewhere. Never in read-only mode — an
    // inspect-only instance must not kill anything.
    if !crate::read_only() {
        let report = crate::agent_reaper::reap_orphans(&platform::state_base_dir());
        if report.killed > 0 {
            info!(
                "reaped {} orphan agent process(es) (~{} MB) leaked by a prior run",
                report.killed, report.freed_mb
            );
        }
    }

    Application::new().run(|cx: &mut App| {
        let prefs = load_preferences(&platform::config_dir());
        let keycodes: Vec<u8> = if prefs.hotkeys.is_empty() {
            DEFAULT_HOTKEYS.to_vec()
        } else {
            prefs.hotkeys
        };

        let window_handle = cx.open_window(
            WindowOptions {
                titlebar: None,
                window_background: WindowBackgroundAppearance::Transparent,
                // Sets the X11 `WM_CLASS` (and Wayland app-id) to match the
                // `.desktop` file's `StartupWMClass=tab-atelier`, so the
                // running window is tied to `tab-atelier.desktop` and the
                // taskbar/dock shows its `Icon=tab-atelier`. Without this
                // gpui leaves the class unset and the window gets a generic
                // fallback icon.
                app_id: Some("tab-atelier".to_owned()),
                ..Default::default()
            },
            |window, cx| {
                window.toggle_fullscreen();
                cx.new(|cx| AppState::new(window, cx))
            },
        );
        // Without a window there's no app; report it and exit cleanly (a normal
        // exit code, not a panic + backtrace).
        let window_handle = match window_handle {
            Ok(h) => h,
            Err(e) => {
                error!("cannot open the main window: {e}");
                std::process::exit(1);
            }
        };

        spawn_hotkey_listener(&keycodes, window_handle, cx);
    });
}

/// Guake toggle decision for a hotkey press: SHOW (raise) the window unless it
/// is already the visible, foreground window — in which case hide it.
///
/// Raising a window that's `visible` but NOT the active one (e.g. it's behind a
/// browser opened by clicking a link) is what fixes the "press the hotkey twice
/// to get it back" bug: a naive `!visible` flip would first minimise the
/// already-behind window instead of revealing it.
const fn hotkey_should_show(visible: bool, window_active: bool) -> bool {
    !visible || !window_active
}

fn spawn_hotkey_listener(keycodes: &[u8], window_handle: WindowHandle<AppState>, cx: &mut App) {
    // An awaitable channel, not a polled std::mpsc: the old loop woke
    // 20×/s forever to try_recv a hotkey that fires a few times an hour.
    // tokio's unbounded channel is runtime-free (no reactor needed), so
    // gpui's executor can await it and the loop runs ONLY on keypresses.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let handle = platform::grab_hotkeys(keycodes, move || {
        let _ = tx.send(());
    });

    let _ = window_handle.update(cx, |state, _window, _cx| {
        state.hotkey_handle = Some(handle);
    });

    cx.spawn(async move |cx: &mut AsyncApp| {
        while rx.recv().await.is_some() {
            let _ = cx.update(|cx| {
                let _ = window_handle.update(cx, |state, window, app_cx| {
                    // Toggle from the ACTUAL window state, not just our `visible`
                    // flag. Clicking a link opens a browser on top of us: we stay
                    // `visible == true` but are no longer the foreground window,
                    // so a plain flip would minimise the already-behind window
                    // (press 1) and only reveal it on press 2. Raising a
                    // visible-but-unfocused window instead makes one press bring
                    // us back.
                    let show = hotkey_should_show(state.visible, window.is_window_active());
                    state.visible = show;
                    state.visible_flag.store(show, std::sync::atomic::Ordering::Relaxed);
                    if show {
                        let active = state.active;
                        state.tabs[active].activate();
                        // While hidden, the active tab's repaint pump parks and its
                        // frame cache goes stale — remote-driven output or an
                        // in-place TUI redraw that happened while we were down was
                        // never painted. Drop the cache + notify so `render()`
                        // rebuilds every row from the live grid on reveal; without
                        // this the drop-down shows the frame from when it was
                        // hidden (stale, and the bottom isn't cleaned) until a
                        // keystroke re-damages it.
                        state.tabs[active].view.update(app_cx, |v, vcx| {
                            v.release_render_caches();
                            vcx.notify();
                        });
                        window.activate_window();
                    } else {
                        state.tabs[state.active].deactivate();
                        window.minimize_window();
                    }
                });
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_view(cx: &mut gpui::TestAppContext) -> Entity<TerminalView> {
        let window = cx.add_window(|window, cx| {
            TerminalView::new_with_colors_and_env(
                None,
                FontConfig::default(),
                Rc::new(RefCell::new(None)),
                Rc::new(RefCell::new(None)),
                true,
                std::collections::HashMap::new(),
                None,
                Some((80, 24, gpui::size(gpui::px(8.0), gpui::px(16.0)))),
                true, // skeleton — no shell needed to seed a Tab
                window,
                cx,
            )
        });
        window.update(cx, |_, _, cx| cx.entity()).unwrap()
    }

    #[gpui::test]
    fn from_state_seeds_a_restored_tab(cx: &mut gpui::TestAppContext) {
        let view = test_view(cx);
        let ts = TabState {
            id: "tab-42".to_string(),
            name: "builds".to_string(),
            uptime_secs: Some(90.0),
            agent_session_id: Some("sess-1".to_string()),
            agent_kind: Some("claude".to_string()),
            agent_plan_mode: Some(true),
            share_token_rw: "rw-tok".to_string(),
            share_token_ro: "ro-tok".to_string(),
            locked: true,
            bg_color: Some("#112233".to_string()),
            ..TabState::default()
        };
        let tab = Tab::from_state(
            view,
            &ts,
            Some(PathBuf::from("/tmp/somewhere")),
            Some("saved scrollback".to_string()),
            Some("claude --resume sess-1\n".to_string()),
            false,
        );
        assert_eq!(&*tab.id, "tab-42");
        assert_eq!(&*tab.name, "builds");
        assert_eq!(tab.prior_uptime, std::time::Duration::from_secs(90));
        assert!(tab.last_activated.is_none(), "restored tabs boot deactivated");
        assert!(!tab.unreviewed_work, "restored tabs boot grey");
        assert!(
            tab.last_focused_at.is_some(),
            "boots 'just seen' so it doesn't flash dormant"
        );
        assert_eq!(tab.agent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(tab.agent_kind.as_deref(), Some("claude"));
        assert_eq!(tab.agent_plan_mode, Some(true));
        assert_eq!(&*tab.share_token_rw, "rw-tok");
        assert_eq!(&*tab.share_token_ro, "ro-tok");
        assert!(tab.locked);
        assert_eq!(tab.bg_color.as_deref(), Some("#112233"));
        assert_eq!(tab.last_known_cwd_string.as_deref(), Some("/tmp/somewhere"));
        assert_eq!(tab.last_known_cwd, Some(PathBuf::from("/tmp/somewhere")));
        assert_eq!(tab.pending_restore.as_deref(), Some("saved scrollback"));
        assert!(tab.pending_agent_resume.is_some());
        assert!(tab.context.is_none());
        assert!(tab.snap_cache.is_none());
    }

    #[gpui::test]
    fn from_state_seeds_a_fresh_active_tab(cx: &mut gpui::TestAppContext) {
        let view = test_view(cx);
        let seed = TabState {
            id: "fresh-id".to_string(),
            name: "Terminal 3".to_string(),
            ..TabState::default()
        };
        let tab = Tab::from_state(view, &seed, None, None, None, true);
        assert_eq!(&*tab.id, "fresh-id");
        assert_eq!(&*tab.name, "Terminal 3");
        assert!(tab.last_activated.is_some(), "fresh tabs are foreground from birth");
        assert_eq!(tab.prior_uptime, std::time::Duration::ZERO);
        assert!(tab.share_token_rw.is_empty() && tab.share_token_ro.is_empty());
        assert!(!tab.locked && tab.schedule.is_none() && tab.bg_color.is_none());
        assert!(tab.agent_session_id.is_none() && tab.agent_kind.is_none());
        assert!(tab.last_known_cwd.is_none() && tab.last_known_cwd_string.is_none());
    }

    /// Drives the saver thread through its three verdicts: write, skip
    /// via unchanged ring (serialize must not even run), skip via
    /// unchanged content crc. `t2` jobs double as ordering fences —
    /// the channel is FIFO, so once t2's effect is visible t1's
    /// preceding job has been fully processed.
    #[test]
    fn output_saver_dedups_by_ring_then_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let saver = OutputSaver::spawn(base.clone());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let job = |name: &str, ring: u64, out: &str| -> SaveJob {
            let calls = calls.clone();
            let out = out.to_string();
            SaveJob {
                name: name.into(),
                ring_len: ring,
                serialize: Box::new(move || {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    out
                }),
            }
        };
        // Generous ceiling: the saver fsyncs, and a fully parallel test
        // run can starve it well past a "reasonable" couple of seconds.
        let wait_for = |pred: &dyn Fn() -> bool| {
            for _ in 0..1000 {
                if pred() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };
        // The worker jumps to the LATEST queued batch (saves are
        // idempotent), so each phase rides in a single batch and the
        // trailing `t2` job doubles as the completion fence.
        saver.tx.send(vec![job("t1", 1, "hello")]).unwrap();
        assert!(
            wait_for(&|| crate::load_tab_output(&base, "t1").is_some()),
            "first job writes the file"
        );
        assert_eq!(crate::load_tab_output(&base, "t1").as_deref(), Some("hello"));
        // Same ring_len → the scrollback serialize is skipped entirely.
        saver
            .tx
            .send(vec![job("t1", 1, "SHOULD NOT RUN"), job("t2", 1, "fence")])
            .unwrap();
        assert!(wait_for(&|| crate::load_tab_output(&base, "t2").is_some()));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "unchanged ring must not pay the serialize"
        );
        assert_eq!(crate::load_tab_output(&base, "t1").as_deref(), Some("hello"));
        // New ring but identical bytes → serialized once more, but the
        // crc gate stops the rewrite.
        saver
            .tx
            .send(vec![job("t1", 2, "hello"), job("t2", 2, "fence2")])
            .unwrap();
        assert!(wait_for(
            &|| crate::load_tab_output(&base, "t2").as_deref() == Some("fence2")
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(crate::load_tab_output(&base, "t1").as_deref(), Some("hello"));
    }

    #[test]
    fn agent_led_hidden_for_a_dead_session_with_nothing_to_review() {
        use crate::{AgentState, TabLed, compute_tab_led};
        // Live agent running, session attached, nothing to review → grey Idle.
        // (non-daemon: is_daemon=false, daemon_alive ignored.)
        assert_eq!(
            compute_tab_led(None, true, false, true, false, false, false, true),
            Some(TabLed::Idle)
        );
        // A transient state always shows (a hook just fired) even if not alive.
        assert_eq!(
            compute_tab_led(Some(AgentState::Waiting), true, false, false, false, false, false, true),
            Some(TabLed::Idle)
        );
        // The reported bug: durable anchor attached, but the agent never
        // restarted and nothing to review, no state — and no sweep has yet run,
        // so it isn't claimed dead either → NO LED.
        assert_eq!(
            compute_tab_led(None, true, false, false, false, false, false, true),
            None
        );
        // Once a full sweep confirms the process is gone → dim-red Dead dot.
        assert_eq!(
            compute_tab_led(None, true, false, false, true, false, false, true),
            Some(TabLed::Dead)
        );
        // No session at all → never.
        assert_eq!(
            compute_tab_led(None, false, false, true, false, false, false, true),
            None
        );
    }

    #[test]
    fn daemon_led_is_up_down_from_real_process_liveness() {
        use crate::{TabLed, compute_tab_led};
        // A daemon (brain/aligator) LED reflects REAL process liveness and is
        // ALWAYS visible — up=Working (green), down=Dead (red). is_daemon=true,
        // last arg = daemon_alive. The agent-session inputs are irrelevant here.
        // brain: alive → Working, dead → Dead.
        assert_eq!(
            compute_tab_led(None, true, true, false, false, false, false, true),
            Some(TabLed::Working),
            "a live brain daemon is green, not falsely dead"
        );
        assert_eq!(
            compute_tab_led(None, true, true, false, true, false, false, false),
            Some(TabLed::Dead),
            "a stopped brain daemon is red"
        );
        // aligator: alive → Working (the reported bug — it was red while running),
        // dead → Dead. Same palette as everything else.
        assert_eq!(
            compute_tab_led(None, true, true, false, true, false, false, true),
            Some(TabLed::Working),
            "a live aligator daemon is green (fixes the false-red)"
        );
        assert_eq!(
            compute_tab_led(None, false, true, false, false, false, false, false),
            Some(TabLed::Dead),
            "a stopped aligator daemon is red — always visible, never None"
        );
        // A daemon is NEVER hidden (up or down), regardless of session inputs.
        assert!(
            compute_tab_led(None, false, true, false, false, false, false, true).is_some(),
            "a daemon always shows a dot"
        );
    }

    #[test]
    fn hotkey_toggle_raises_a_visible_but_unfocused_window() {
        // Foreground + visible → the hotkey hides it (normal Guake toggle).
        assert!(!hotkey_should_show(true, true));
        // Visible but NOT focused (a browser opened from a link is on top) →
        // RAISE, not hide — this is the one-press-not-two fix.
        assert!(hotkey_should_show(true, false));
        // Hidden/minimised → show, regardless of the stale active bit.
        assert!(hotkey_should_show(false, false));
        assert!(hotkey_should_show(false, true));
    }

    #[test]
    fn ram_gauge_fill_reddens_near_the_cap() {
        // Comfortable (<75%) → blue; getting close (75–90%) → amber; near the
        // cap (>=90%) → red. Compare the dominant channel to keep it robust.
        let blue = ram_gauge_fill(0.5);
        assert!(blue.b > blue.r, "low usage should be blue-dominant");
        let amber = ram_gauge_fill(0.8);
        assert!(amber.r > amber.b && amber.g > amber.b, "mid usage should be amber");
        let red = ram_gauge_fill(0.95);
        assert!(red.r > red.g && red.r > red.b, "high usage should be red-dominant");
        // 0.9 is the start of the red band: red's green channel is much lower
        // than amber's, so this distinguishes the two without an f32 == compare.
        assert!(ram_gauge_fill(0.9).g < 0.5, "0.9 should already be in the red band");
        assert!(ram_gauge_fill(0.89).g > 0.5, "just under 0.9 is still amber");
    }

    #[test]
    fn grid_dims_fits_viewport_minus_tab_bar() {
        // 800×600 window, 8×16 px cells → 100 cols; height minus the 32px tab
        // bar is 568 / 16 = 35 lines (truncated).
        assert_eq!(grid_dims(800.0, 600.0, 8.0, 16.0), Some((100, 35)));
        // A wider cell yields fewer columns.
        assert_eq!(grid_dims(800.0, 600.0, 10.0, 16.0), Some((80, 35)));
    }

    #[test]
    fn grid_dims_rejects_unlaid_out_or_unmeasured() {
        // Zero viewport (window not laid out yet) → fall back to 80×24 spawn.
        assert_eq!(grid_dims(0.0, 0.0, 8.0, 16.0), None);
        assert_eq!(grid_dims(800.0, 0.0, 8.0, 16.0), None);
        // Unmeasured cell.
        assert_eq!(grid_dims(800.0, 600.0, 0.0, 16.0), None);
    }

    #[test]
    fn grid_dims_clamps_to_a_minimum_grid() {
        // A viewport smaller than the tab bar + one cell still yields a usable
        // grid rather than 0 lines / <2 cols.
        let (cols, lines) = grid_dims(5.0, 10.0, 8.0, 16.0).expect("some");
        assert!(cols >= 2 && lines >= 1);
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(std::time::Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(std::time::Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(std::time::Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(std::time::Duration::from_mins(1)), "1m 0s");
        assert_eq!(format_duration(std::time::Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(std::time::Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(std::time::Duration::from_hours(1)), "1h 0m");
        assert_eq!(format_duration(std::time::Duration::from_mins(121)), "2h 1m");
        assert_eq!(format_duration(std::time::Duration::from_hours(24)), "24h 0m");
    }

    #[test]
    fn mru_tab_order_excludes_active_recent_first_none_last() {
        // Fake "instants" as u64 (larger = more recent). Active tab (2) is
        // dropped; the rest sort newest→oldest, and the never-focused tab (3)
        // trails.
        let ts = vec![Some(10u64), Some(30), Some(99), None, Some(20)];
        assert_eq!(mru_tab_order(2, &ts), vec![1, 4, 0, 3]);
        // A single-tab window yields an empty list (nothing to switch to).
        assert_eq!(mru_tab_order(0, &[Some(5u64)]), Vec::<usize>::new());
        // All never-focused → original order preserved (stable), active gone.
        assert_eq!(mru_tab_order(1, &[None::<u64>, None, None]), vec![0, 2]);
    }
}
