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
use crate::theme::{self, ThemeName};
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
use crate::{api_url_for_local_clients, build_agent_resume_command, tab_env_extras};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Application, AsyncApp, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowHandle, WindowOptions, div, px, rgba,
};
use log::{debug, error, info, warn};

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
const STREAMING_LED_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

struct Tab {
    view: Entity<TerminalView>,
    name: String,
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
    last_known_cwd_string: Option<String>,
    /// Stable per-tab UUID — sourced from `TabState.id` on first
    /// load, generated fresh on tab creation. Exported into the
    /// shell as `_TAB_ID` so tools can call `POST /tabs/by-id/{id}/
    /// status` without caring about renames.
    id: String,
    /// Transient agent status published by a tool inside the tab
    /// (via the local API). Drives the tab-strip LED. Cleared by
    /// the staleness sweep after 5 minutes of no updates.
    agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable: last agent session UUID associated with this tab.
    /// Persisted to tabs.json so auto-resume can pick the same
    /// session back up after a restart.
    agent_session_id: Option<String>,
    /// Durable: which agent CLI owns the session ("catbus" or
    /// "claude" today). Free-form string so future agents can
    /// register without a code change. Used by the resume path
    /// to decide which command to type.
    agent_kind: Option<String>,
    /// Durable: whether the agent was in plan / read-only mode
    /// at last save. Restored along with the session so the tab
    /// comes back in the same mode.
    agent_plan_mode: Option<bool>,
    /// Per-tab share secrets. Minted lazily by the right-click
    /// share-link menu and persisted to tabs.json so URLs survive
    /// restarts. Empty until first share.
    share_token_rw: String,
    share_token_ro: String,
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
    context: Option<String>,
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
            let mut bytes = cmd.into_bytes();
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
    name: String,
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
            .child(self.name.clone())
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
const LOGO_PNG: &[u8] = include_bytes!("../assets/icons/icon-192.png");

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

/// One tab's output-save request for the [`OutputSaver`] worker: its name, the
/// ring length (a cheap dirtiness key), and a `Send` closure that serialises the
/// scrollback. The main thread builds these (an `Arc` clone + a brief ring lock
/// per tab); the worker runs the expensive serialize + atomic disk write.
struct SaveJob {
    name: String,
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
                let mut seen: std::collections::HashMap<String, (u64, u32)> = std::collections::HashMap::new();
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
    opacity: u8,
    hotkeys: Vec<u8>,
    show_preferences: bool,
    show_hotkey_picker: bool,
    hotkey_picker_focus: FocusHandle,
    hotkey_picker_error: Option<String>,
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
        let pref_browser_focus = cx.focus_handle();
        let pref_editor_focus = cx.focus_handle();
        let pref_api_addr_focus = cx.focus_handle();
        let pref_api_tls_addr_focus = cx.focus_handle();
        let pref_share_url_base_focus = cx.focus_handle();
        let prefs = load_preferences(&platform::config_dir());
        // Per-tab cgroup ceilings (Linux). Cloned before `prefs` fields
        // are moved below; layered under each tab's own limits at spawn.
        #[cfg(target_os = "linux")]
        let default_limits = prefs.default_tab_limits.clone();
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

        let (tabs, active, restored_windowed) = if let Some(mut saved) =
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
                let env = tab_env_extras(&ts.id, &api_url_for_pty, &api_token);
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
                if ts.net_disabled && crate::bwrap_available() {
                    view.update(cx, |v, _| {
                        v.set_net_disabled(true);
                        v.respawn(cwd.as_deref());
                    });
                }
                // Auto-resume: if this tab had an agent session and kind
                // persisted, queue the resume command to be typed into the
                // freshly-spawned shell — UNLESS we already launched the
                // agent directly above (then typing it would double-launch).
                let pending_agent_resume = if agent_launch.is_some() || crate::read_only() {
                    None
                } else {
                    match (&ts.agent_kind, &ts.agent_session_id) {
                        (Some(kind), Some(sid)) => build_agent_resume_command(kind, sid, ts.agent_plan_mode),
                        _ => None,
                    }
                };
                tabs.push(Tab {
                    view,
                    id: ts.id.clone(),
                    name: ts.name.clone(),
                    created_at: std::time::Instant::now(),
                    prior_uptime: std::time::Duration::from_secs_f64(ts.uptime_secs.unwrap_or(0.0)),
                    active_duration: std::time::Duration::ZERO,
                    last_activated: None,
                    // Boots un-flagged (grey): it only goes blue once its
                    // agent WORKS while you're not looking. Restoring a tab
                    // isn't "new work", so it must not flash blue on restart.
                    unreviewed_work: false,
                    last_focused_at: Some(std::time::Instant::now()),
                    last_output_at: None,
                    #[cfg(feature = "energy")]
                    energy_wh: ts.energy_wh.unwrap_or(0.0),
                    #[cfg(feature = "energy")]
                    energy_wh_last_saved: ts.energy_wh.unwrap_or(0.0),
                    #[cfg(feature = "catbus")]
                    tokens_last_saved: std::cell::Cell::new(None),
                    #[cfg(feature = "catbus")]
                    tokens_last_ring: std::cell::Cell::new(0),
                    uptime_last_saved: std::cell::Cell::new(None),
                    led_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    sweep_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    agent_pid: std::cell::Cell::new(None),
                    // Seed with the hash of the just-restored output so the
                    // first persist tick after launch doesn't rewrite an
                    // identical file.
                    pending_restore,
                    // Seed from saved state so an immediate persist tick
                    // before the new shell has a /proc/PID/cwd readable
                    // doesn't overwrite the restored value with None.
                    last_known_cwd_string: cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    last_known_cwd: cwd.clone(),
                    agent_state: None,
                    agent_session_id: ts.agent_session_id.clone(),
                    agent_kind: ts.agent_kind.clone(),
                    agent_plan_mode: ts.agent_plan_mode,
                    share_token_rw: ts.share_token_rw.clone(),
                    share_token_ro: ts.share_token_ro.clone(),
                    locked: ts.locked,
                    schedule: ts.schedule.clone(),
                    bg_color: ts.bg_color.clone(),
                    context: None,
                    last_pushed_locked: None,
                    pending_agent_resume,
                    snap_cache: None,
                    limits: ts.limits.clone(),
                });
            }
            if tabs.is_empty() {
                let fc = font_config.clone();
                let br = browser.clone();
                let ce = code_editor.clone();
                let new_id = crate::default_tab_id();
                let env = tab_env_extras(&new_id, &api_url_for_pty, &api_token);
                let view = cx.new(|cx| {
                    let mut tv = TerminalView::new_with_colors_and_env(
                        None, fc, br, ce, true, env, None, boot_grid, false, window, cx,
                    );
                    tv.set_theme(theme_name);
                    tv
                });
                tabs.push(Tab {
                    view,
                    name: locale::strings(lang).terminal.into(),
                    created_at: std::time::Instant::now(),
                    prior_uptime: std::time::Duration::ZERO,
                    active_duration: std::time::Duration::ZERO,
                    last_activated: None,
                    // Boots un-flagged (grey): it only goes blue once its
                    // agent WORKS while you're not looking. Restoring a tab
                    // isn't "new work", so it must not flash blue on restart.
                    unreviewed_work: false,
                    last_focused_at: Some(std::time::Instant::now()),
                    last_output_at: None,
                    #[cfg(feature = "energy")]
                    energy_wh: 0.0,
                    #[cfg(feature = "energy")]
                    energy_wh_last_saved: 0.0,
                    #[cfg(feature = "catbus")]
                    tokens_last_saved: std::cell::Cell::new(None),
                    #[cfg(feature = "catbus")]
                    tokens_last_ring: std::cell::Cell::new(0),
                    uptime_last_saved: std::cell::Cell::new(None),
                    led_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    sweep_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    agent_pid: std::cell::Cell::new(None),
                    pending_restore: None,
                    last_known_cwd: None,
                    last_known_cwd_string: None,
                    id: new_id,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    agent_plan_mode: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    schedule: None,
                    bg_color: None,
                    context: None,
                    last_pushed_locked: None,
                    pending_agent_resume: None,
                    snap_cache: None,
                    limits: crate::TabResourceLimits::default(),
                });
            }
            let active = saved.active.min(tabs.len() - 1);
            tabs[active].activate();
            (tabs, active, saved.windowed)
        } else {
            let fc = font_config.clone();
            let br = browser.clone();
            let ce = code_editor.clone();
            let new_id = crate::default_tab_id();
            let env = tab_env_extras(&new_id, &api_url_for_pty, &api_token);
            let view = cx.new(|cx| {
                let mut tv = TerminalView::new_with_colors_and_env(
                    None, fc, br, ce, true, env, None, boot_grid, false, window, cx,
                );
                tv.set_theme(theme_name);
                tv
            });
            (
                vec![Tab {
                    view,
                    name: locale::strings(lang).terminal.into(),
                    created_at: std::time::Instant::now(),
                    prior_uptime: std::time::Duration::ZERO,
                    active_duration: std::time::Duration::ZERO,
                    last_activated: Some(std::time::Instant::now()),
                    unreviewed_work: false,
                    last_focused_at: Some(std::time::Instant::now()),
                    last_output_at: None,
                    #[cfg(feature = "energy")]
                    energy_wh: 0.0,
                    #[cfg(feature = "energy")]
                    energy_wh_last_saved: 0.0,
                    #[cfg(feature = "catbus")]
                    tokens_last_saved: std::cell::Cell::new(None),
                    #[cfg(feature = "catbus")]
                    tokens_last_ring: std::cell::Cell::new(0),
                    uptime_last_saved: std::cell::Cell::new(None),
                    led_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    sweep_last_ring: std::cell::Cell::new(0),
                    #[cfg(feature = "catbus")]
                    agent_pid: std::cell::Cell::new(None),
                    pending_restore: None,
                    last_known_cwd: None,
                    last_known_cwd_string: None,
                    id: new_id,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    agent_plan_mode: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    schedule: None,
                    bg_color: None,
                    context: None,
                    last_pushed_locked: None,
                    pending_agent_resume: None,
                    snap_cache: None,
                    limits: crate::TabResourceLimits::default(),
                }],
                0,
                false,
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
                        let mut spawned = 0;
                        for tab in &app.tabs {
                            if spawned >= 2 {
                                break;
                            }
                            if !tab.view.read(cx).is_spawned() {
                                tab.view.update(cx, |v, _| v.ensure_spawned());
                                spawned += 1;
                            }
                        }
                        app.tabs.iter().all(|t| t.view.read(cx).is_spawned())
                    })
                    .unwrap_or(true);
                if done {
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
            pending_bg_color_changes: Vec::new(),
            pending_context_changes: Vec::new(),
            pending_token_rotations: Vec::new(),
            pending_schedule_changes: Vec::new(),
            pending_new_tabs: 0,
            pending_new_tab_cwds: std::collections::VecDeque::new(),
            pending_limit_changes: Vec::new(),
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
            opacity,
            hotkeys,
            show_preferences: false,
            show_hotkey_picker: false,
            hotkey_picker_focus,
            hotkey_picker_error: None,
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
            if i == self.active {
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
        let new_id = crate::default_tab_id();
        let env = tab_env_extras(&new_id, &api_url_for_local_clients(&self.api_addr), &self.api_token);
        let view = cx.new(|cx| {
            // Fresh tab — no agent session yet, so a plain shell (None). Spawn at
            // the live window's grid so the tab is correctly sized immediately.
            let mut tv = TerminalView::new_with_colors_and_env(
                cwd.as_deref(),
                fc,
                br,
                ce,
                true,
                env,
                None,
                grid,
                false,
                window,
                cx,
            );
            tv.set_theme(tn);
            tv
        });
        let idx = at.min(self.tabs.len());
        self.tabs.insert(
            idx,
            Tab {
                view,
                name: format!("{} {}", self.t().terminal_n, self.tabs.len()),
                created_at: std::time::Instant::now(),
                prior_uptime: std::time::Duration::ZERO,
                active_duration: std::time::Duration::ZERO,
                last_activated: Some(std::time::Instant::now()),
                unreviewed_work: false,
                last_focused_at: Some(std::time::Instant::now()),
                last_output_at: None,
                #[cfg(feature = "energy")]
                energy_wh: 0.0,
                #[cfg(feature = "energy")]
                energy_wh_last_saved: 0.0,
                #[cfg(feature = "catbus")]
                tokens_last_saved: std::cell::Cell::new(None),
                #[cfg(feature = "catbus")]
                tokens_last_ring: std::cell::Cell::new(0),
                uptime_last_saved: std::cell::Cell::new(None),
                led_last_ring: std::cell::Cell::new(0),
                #[cfg(feature = "catbus")]
                sweep_last_ring: std::cell::Cell::new(0),
                #[cfg(feature = "catbus")]
                agent_pid: std::cell::Cell::new(None),
                pending_restore: None,
                last_known_cwd_string: cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                last_known_cwd: cwd,
                id: new_id,
                agent_state: None,
                agent_session_id: None,
                agent_kind: None,
                agent_plan_mode: None,
                share_token_rw: String::new(),
                share_token_ro: String::new(),
                locked: false,
                schedule: None,
                bg_color: None,
                context: None,
                last_pushed_locked: None,
                pending_agent_resume: None,
                snap_cache: None,
                limits: crate::TabResourceLimits::default(),
            },
        );
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

    fn persist(&mut self, cx: &mut Context<Self>) {
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
                tab.last_known_cwd_string = Some(p.to_string_lossy().into_owned());
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
                .map(|tab| (tab.id.clone(), tab.view.read(cx).pid()))
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
                let cwd = tab.last_known_cwd_string.clone();
                TabState {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    cwd,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                    net_disabled: tab.view.read(cx).net_disabled(),
                    agent_session_id: tab.agent_session_id.clone(),
                    agent_kind: tab.agent_kind.clone(),
                    agent_plan_mode: tab.agent_plan_mode,
                    share_token_rw: tab.share_token_rw.clone(),
                    share_token_ro: tab.share_token_ro.clone(),
                    locked: tab.locked,
                    schedule: tab.schedule.clone(),
                    bg_color: tab.bg_color.clone(),
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
            let bg_color = crate::effective_tab_bg(tab.bg_color.as_deref(), self.tab_bg_global.as_deref()).to_string();
            api_tabs.push(api::SnapshotTab {
                id: tab.id.clone(),
                name: ts.name.clone(),
                cwd: ts.cwd.clone(),
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
                share_token_rw: ts.share_token_rw.clone(),
                share_token_ro: ts.share_token_ro.clone(),
                locked: ts.locked,
                schedule: ts.schedule.clone(),
                bg_color,
                context: tab.context.clone(),
                shell_pid,
                agent_state: tab.agent_state.clone(),
                agent_session_id: tab.agent_session_id.clone(),
                agent_kind: tab.agent_kind.clone(),
                viewers: pty_ring.lock().map_or(0, |r| r.viewer_count()),
                pty_ring: Some(pty_ring),
                net_disabled: ts.net_disabled,
                connections: self
                    .tab_connections
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&tab.id)
                    .copied()
                    .unwrap_or(0),
                // Desktop is unprivileged → no nft byte counters.
                tx_bytes: 0,
                tx_denied_bytes: 0,
                // Desktop allowlist isn't wired (headless-only feature).
                net_allow: crate::net_policy::AllowConfig::default(),
                dns_entries: Vec::new(),
            });
        }

        let read_only = crate::read_only();
        let saved = SavedState {
            tabs,
            active: self.active,
            windowed: self.windowed,
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
            let token_rotations: Vec<String> = snapshot.pending_token_rotations.drain(..).collect();
            let schedule_changes: Vec<(String, Option<crate::schedule::TabSchedule>)> =
                snapshot.pending_schedule_changes.drain(..).collect();
            let limit_changes: Vec<(String, crate::TabResourceLimits, bool)> =
                snapshot.pending_limit_changes.drain(..).collect();
            drop(snapshot);
            // Apply lock toggles from the API/CLI onto the runtime
            // Tab's manual flag. The view's set_locked() push happens
            // in the per-tick mirror below — that's the single site
            // that funnels `effective_locked()` into the gpui view,
            // so a future caller can't accidentally toggle the view
            // without also covering schedule-driven locks.
            for (tab_id, locked) in lock_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
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
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    let cwd = platform::process_cwd(tab.view.read(cx).pid()).or_else(|| std::env::current_dir().ok());
                    tab.view.update(cx, |v, _| {
                        v.set_net_disabled(disabled);
                        v.respawn(cwd.as_deref());
                    });
                }
            }
            for (tab_id, color) in bg_color_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.bg_color = color;
                }
            }
            // Revoke per-tab share tokens on the runtime Tab so the
            // cleared state persists into tabs.json (the snapshot was
            // already cleared by the endpoint for instant 401s).
            for tab_id in token_rotations {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.share_token_rw.clear();
                    tab.share_token_ro.clear();
                }
            }
            for (tab_id, context) in context_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.context = context;
                }
            }
            // Schedule changes — None clears, Some sets. Mirrors the
            // `locked` / `bg_color` drain above: mutate the runtime
            // `Tab` so the next persist tick rebuilds `tabs.json` +
            // `api_tabs` with the new value.
            for (tab_id, sched) in schedule_changes {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
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
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
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
                let Some(tab) = self.tabs.iter_mut().find(|t| t.id == upd.tab_id) else {
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
                        tab.agent_session_id = upd.session_id;
                    }
                    if upd.agent_kind.is_some() {
                        tab.agent_kind = upd.agent_kind;
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
                let reviewed = i == active || tab.view.read(cx).viewer_count() > 0;
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
                    live_agents.push((pid, tab.agent_session_id.clone().unwrap_or_default()));
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
            for tab in &mut self.tabs {
                if tab.pending_agent_resume.is_some() && tab.view.read(cx).ring_len() > 0 {
                    tab.flush_pending_agent_resume(cx);
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
        let env = tab_env_extras(
            &self.tabs[idx].id,
            &api_url_for_local_clients(&self.api_addr),
            &self.api_token,
        );
        // Respawning an agent tab → relaunch the agent directly (exec), same as
        // a restore, so it comes back as claude rather than a bare shell. Never
        // in read-only mode — see the restore path: resuming a live session
        // corrupts the user's session ids.
        let agent_launch = if crate::clear_env() && !crate::read_only() {
            match (&self.tabs[idx].agent_kind, &self.tabs[idx].agent_session_id) {
                (Some(k), Some(s)) => {
                    // Name the agent process after the tab (see the restore path).
                    let title = crate::shell_supports_exec_a(&crate::clear_env_shell_path())
                        .then_some(self.tabs[idx].name.as_str());
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
        if old_name == new_name {
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
        self.tabs[idx].name = new_name;
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let state_base = platform::state_base_dir();
        // Snapshot cwd from /proc one last time before child processes
        // disappear; fall back to the cached last_known_cwd otherwise.
        for tab in &mut self.tabs {
            let pid = tab.view.read(cx).pid();
            if let Some(p) = platform::process_cwd(pid)
                && tab.last_known_cwd.as_deref() != Some(p.as_path())
            {
                tab.last_known_cwd_string = Some(p.to_string_lossy().into_owned());
                tab.last_known_cwd = Some(p);
            }
        }
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let cwd = tab.last_known_cwd_string.clone();
                TabState {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    cwd,
                    colors_enabled: tab.view.read(cx).colors_enabled(),
                    net_disabled: tab.view.read(cx).net_disabled(),
                    agent_session_id: tab.agent_session_id.clone(),
                    agent_kind: tab.agent_kind.clone(),
                    agent_plan_mode: tab.agent_plan_mode,
                    share_token_rw: tab.share_token_rw.clone(),
                    share_token_ro: tab.share_token_ro.clone(),
                    locked: tab.locked,
                    bg_color: tab.bg_color.clone(),
                    limits: tab.limits.clone(),
                    ..TabState::default()
                }
            })
            .collect();
        if !crate::read_only() {
            // Drain queued periodic writes FIRST so none of them can land
            // after (and clobber) the final synchronous state below.
            self.state_writer.flush();
            save_state(
                &platform::config_base_dir(),
                &SavedState {
                    tabs,
                    active: self.active,
                    windowed: self.windowed,
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
            self.tabs[self.active].name.clone()
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

    fn render_tab_bar(&mut self, battery: Option<u8>, _window: &mut Window, cx: &mut Context<Self>) -> Stateful<Div> {
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
        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            // Visual lock marker — a 🔒 ahead of the name is enough to
            // make "this tab won't accept input" obvious at a glance,
            // and stays out of the rename text (still raw name in the
            // rename editor).
            let base_name = if let Some((ri, ref text)) = self.renaming {
                if ri == i { text.clone() } else { tab.name.clone() }
            } else {
                tab.name.clone()
            };
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
            // Is the agent PROCESS actually running? The catbus sweep stamps
            // `agent_pid` = Some when it finds a live `claude`/`catbus-agent`
            // descendant, None when it's Gone. Without the sweep (catbus off) we
            // can't tell, so assume alive and keep the old anchor-based LED.
            #[cfg(feature = "catbus")]
            let agent_alive = tab.agent_pid.get().is_some();
            #[cfg(not(feature = "catbus"))]
            let agent_alive = true;
            // A tab whose durable session anchor survived but whose `claude`
            // didn't restart (auto-resume failed / it was killed) no longer lights
            // a "healthy" LED — the anchor stays in tabs.json for a manual resume,
            // but the dot doesn't pretend an agent is there.
            let agent_led = if agent_led_visible(
                tab.agent_state.is_some(),
                session_attached,
                agent_alive || tab.unreviewed_work,
            ) {
                let grey = Hsla::from(Rgba {
                    r: 0.45,
                    g: 0.45,
                    b: 0.45,
                    a: 1.0,
                });
                let thinking_green = Hsla::from(Rgba {
                    r: 0.306,
                    g: 0.788,
                    b: 0.690,
                    a: 1.0,
                });
                let state = tab.agent_state.as_ref().map(|s| s.state);
                // Working = a live thinking hook OR fresh PTY output (a
                // `--resume`d session streams a reply without a thinking hook,
                // and `agent_activity` only counts *child* processes on-CPU, so
                // claude rendering its own reply reads Idle — the output window
                // is what catches it).
                let working = matches!(state, Some(crate::AgentState::Thinking))
                    || tab.last_output_at.is_some_and(|t| t.elapsed() < STREAMING_LED_WINDOW);
                let color = if matches!(state, Some(crate::AgentState::Error)) {
                    Hsla::from(Rgba {
                        r: 0.937,
                        g: 0.267,
                        b: 0.267,
                        a: 1.0,
                    })
                } else if working {
                    thinking_green
                } else if tab.unreviewed_work {
                    Hsla::from(Rgba {
                        r: 0.36,
                        g: 0.60,
                        b: 1.0,
                        a: 1.0,
                    })
                } else {
                    grey
                };
                Some(div().w(px(7.0)).h(px(7.0)).mr(px(5.0)).rounded_full().bg(color))
            } else {
                None
            };

            #[cfg(feature = "energy")]
            let power_label = watts.get(i).map(power::TabPower::label).unwrap_or_default();

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
                .bg(if blink_red {
                    tab_blink_bg
                } else if is_active {
                    tab_active_bg
                } else {
                    tab_bg
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
                            text: ctx.clone(),
                            theme: theme_name,
                        })
                        .into()
                    })
                })
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    if ev.click_count() >= 2 {
                        let name = this.tabs[i].name.clone();
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

            #[cfg(feature = "energy")]
            let tab_el = tab_el.child(
                div()
                    .text_size(px(11.0))
                    .text_color(watts_fg)
                    .min_w(px(55.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(power_label),
            );

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

    /// Summon one more random pet onto the screen (repeated calls grow the herd).
    /// Loads a baked sprite sheet + animation XML from `/usr/share/tab-atelier/pets/`
    /// (dev: `./assets/pets/`). No-op if no pets are installed.
    #[cfg(feature = "pets")]
    fn summon_pet(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        self.pet.summon(f32::from(vp.width), f32::from(vp.height));
    }

    fn render_context_menu(&self, window: &Window, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let menu = self.context_menu.as_ref()?;
        let th = self.th();
        let menu_bg = th.surface_hsla();
        let menu_fg = th.fg_hsla();
        let menu_hover = th.selection_hsla();
        let menu_border = th.border_hsla();

        let pos = menu.position;
        let menu_width = px(150.0);
        let menu_height_estimate = if matches!(menu.kind, MenuKind::Tab(_)) {
            px(400.0)
        } else {
            px(350.0)
        };
        let vp = window.viewport_size();
        let x = pos.x.min(vp.width - menu_width);
        let open_upward = menu.open_upward || pos.y + menu_height_estimate > vp.height;

        let mut container = div().id("context-menu").absolute().left(x);

        container = if open_upward {
            let bottom_offset = vp.height - pos.y;
            container.bottom(bottom_offset)
        } else {
            container.top(pos.y)
        };

        container = container
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
                            let name = this.tabs[idx].name.clone();
                            this.renaming = Some((idx, name));
                            this.rename_select_all = true;
                            this.context_menu = None;
                            this.rename_focus.focus(window);
                            cx.notify();
                        }),
                    )
                    .child(self.t().rename),
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
                                    *slot_ref = crate::mint_share_token();
                                }
                                let token = slot_ref.clone();
                                {
                                    let mut snap =
                                        this.api_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id) {
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
                            this.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child("\u{26d1}\u{fe0f} Brain"),
            );

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
                                if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id_for_lock) {
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
                                    if let Some(t) = snap.tabs.iter_mut().find(|t| t.id == tab_id_for_net) {
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
            let conns = self
                .tab_connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&self.tabs[stats_idx].id)
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

        Some(container)
    }

    fn render_rename_input(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let (_idx, text) = self.renaming.as_ref()?;
        let text = text.clone();
        let th = self.th();
        let input_bg = th.surface_hsla();
        let input_fg = th.fg_hsla();
        let input_border = th.accent_hsla();
        let cursor_color = th.fg_hsla();

        Some(
            div()
                .id("rename-overlay")
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
                        .id("rename-box")
                        .key_context("rename")
                        .track_focus(&self.rename_focus)
                        .bg(input_bg)
                        .border_1()
                        .border_color(input_border)
                        .rounded(px(4.0))
                        .p(px(16.0))
                        .min_w(px(300.0))
                        .text_color(input_fg)
                        .text_size(px(14.0))
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            let key = ev.keystroke.key.as_str();
                            match key {
                                "enter" => {
                                    if let Some((i, ref text)) = this.renaming
                                        && i < this.tabs.len()
                                    {
                                        this.rename_tab(i, text.clone());
                                    }
                                    this.renaming = None;
                                    this.rename_select_all = false;
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "escape" => {
                                    this.renaming = None;
                                    this.rename_select_all = false;
                                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                                    cx.notify();
                                }
                                "backspace" => {
                                    if let Some((_, ref mut text)) = this.renaming {
                                        if this.rename_select_all {
                                            text.clear();
                                            this.rename_select_all = false;
                                        } else {
                                            text.pop();
                                        }
                                    }
                                    cx.notify();
                                }
                                _ => {
                                    if let Some(ref ch) = ev.keystroke.key_char {
                                        if let Some((_, ref mut text)) = this.renaming {
                                            if this.rename_select_all {
                                                text.clear();
                                                this.rename_select_all = false;
                                            }
                                            text.push_str(ch);
                                        }
                                        cx.notify();
                                    }
                                }
                            }
                        }))
                        .child(self.t().rename_tab)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .mt(px(8.0))
                                .bg(th.bg_hsla())
                                .border_1()
                                .border_color(input_border)
                                .rounded(px(3.0))
                                .px(px(8.0))
                                .py(px(4.0))
                                .min_h(px(28.0))
                                .cursor_text()
                                .when(self.rename_select_all, |el| {
                                    el.child(div().bg(th.selection_hsla()).px(px(2.0)).child(text.clone()))
                                })
                                .when(!self.rename_select_all, |el| {
                                    el.child(text).child(div().w(px(1.0)).h(px(16.0)).bg(cursor_color))
                                }),
                        ),
                ),
        )
    }

    fn render_exit_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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

    fn render_close_confirm(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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

    fn render_qr_modal(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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

    fn render_preferences(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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
                        .min_w(px(320.0))
                        .text_size(px(14.0))
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
                        .child(div().text_size(px(16.0)).mb(px(16.0)).child(t.preferences))
                        .child(div().child(t.theme).child(theme_options))
                        .child(div().mt(px(16.0)).child(t.opacity).child(opacity_slider))
                        .child(div().mt(px(16.0)).child(t.toggle_hotkeys).child(hotkey_list))
                        .child(div().mt(px(16.0)).child(t.language).child(lang_options))
                        .child(div().mt(px(16.0)).child(t.browser).child(browser_input))
                        .child(div().mt(px(16.0)).child(t.code_editor).child(editor_input))
                        .child(div().mt(px(16.0)).child(t.api_addr).child(api_addr_input))
                        .child(div().mt(px(16.0)).child(t.api_tls_addr).child(api_tls_addr_input))
                        .child(div().mt(px(16.0)).child(t.share_url_base).child(share_url_base_input))
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
                                                        opacity: Some(this.opacity),
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
                                                        // Headless-only advanced fields set directly
                                                        // in preferences.json; not exposed in the GUI
                                                        // dialog, same treatment as pty_cols above.
                                                        default_tab_limits: crate::TabResourceLimits::default(),
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

    fn render_hotkey_picker(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
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
                        .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, _cx| {})
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

        let mut root = div()
            .id("app-root")
            .size_full()
            .bg(bg_color)
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                if ks.modifiers.control && ks.modifiers.shift && ks.key.as_str() == "t" {
                    this.add_tab_after_current(window, cx);
                    return;
                }
                if ks.modifiers.alt && ks.key.as_str() == "tab" {
                    this.tabs[this.active].deactivate();
                    this.active = (this.active + 1) % this.tabs.len();
                    this.tabs[this.active].activate();
                    this.tabs[this.active].flush_pending_restore(cx);
                    this.tabs[this.active].view.read(cx).focus_handle(cx).focus(window);
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id("terminal-area")
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
                    .child(active_terminal),
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

/// Whether the per-tab agent LED should be shown.
///
/// Visible for a live transient state (`thinking`/`waiting`/`error`), OR an
/// attached session that still has something behind it — `live_or_unreviewed` is
/// "the process is actually running, or it left unreviewed output". NOT for a
/// session whose durable anchor outlived a `claude` that didn't restart (dead +
/// reviewed + no state) — that lit an LED with no agent behind it, which reads as
/// broken. The anchor still persists in `tabs.json` for a manual resume; the dot
/// just stops pretending.
const fn agent_led_visible(has_state: bool, attached: bool, live_or_unreviewed: bool) -> bool {
    has_state || (attached && live_or_unreviewed)
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
                let _ = window_handle.update(cx, |state, window, _cx| {
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
                        state.tabs[state.active].activate();
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

    #[test]
    fn agent_led_hidden_for_a_dead_session_with_nothing_to_review() {
        // Live agent running (or unreviewed output left) → LED on.
        assert!(agent_led_visible(false, true, true));
        // A transient state always shows (a hook just fired).
        assert!(agent_led_visible(true, true, false));
        // The reported bug: durable anchor attached, but the claude never
        // restarted (dead) and nothing to review, no state → NO LED.
        assert!(!agent_led_visible(false, true, false));
        // No session at all → never.
        assert!(!agent_led_visible(false, false, false));
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
}
