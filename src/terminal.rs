// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "gui")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use log::{error, info, trace};

/// Opt-in paint-loop instrument. Set `TAB_ATELIER_PAINT_LOG=1` and the
/// terminal prepaint (Phase 1 cell scan + Phase 2 shaping) wall-time is
/// sampled; every 120 frames an aggregate (mean / p50 / p99 / max +
/// a prepaint-only FPS ceiling) is logged to stderr.
///
/// This is the only way to measure the gpui paint loop — it can't run
/// in the headless `bench` subcommand because the whole loop (`TextRun`,
/// `shape_line`, GPU paint) is gui-feature-only and needs a display. To
/// read real numbers: launch the GUI with the env var set and stress a
/// tab (e.g. `tab-atelier-headless bench`'s paste payload piped in, or
/// `cat` a large file), then watch stderr.
///
/// Note: this times PREPAINT (CPU: cell scan + shaping), not the GPU
/// submit/present that follows. A low prepaint time means the CPU half
/// of the frame isn't the bottleneck; it does not by itself prove the
/// displayed frame rate (the compositor caps that).
mod paint_log {
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::time::Duration;

    fn enabled() -> bool {
        static EN: OnceLock<bool> = OnceLock::new();
        *EN.get_or_init(|| std::env::var_os("TAB_ATELIER_PAINT_LOG").is_some())
    }

    /// One frame's timing breakdown.
    #[derive(Clone, Copy)]
    pub struct Sample {
        /// Total prepaint wall-time.
        pub total: Duration,
        /// Phase 1: Term-lock acquisition + cell scan (under the lock).
        /// High here ⇒ lock contention with the parser or a big scan.
        pub phase1: Duration,
        /// Phase 2: `shape_line` over the segments (outside the lock).
        /// High here ⇒ shaping is the bottleneck (cache misses).
        pub phase2: Duration,
    }

    thread_local! {
        static SAMPLES: RefCell<Vec<Sample>> = const { RefCell::new(Vec::new()) };
        /// GPU paint() phase durations since the last flush.
        static PRESENTS: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
        /// Wall-clock of the first present in the current window, to
        /// compute the actual presents-per-second (proves the 30 fps
        /// sustained-output cap during real play).
        static WINDOW_START: RefCell<Option<std::time::Instant>> = const { RefCell::new(None) };
    }

    const FLUSH_EVERY: usize = 120;

    /// Record one GPU paint()/present wall-time. Separate from the
    /// prepaint sample because the present is where the watts go —
    /// the count over a window gives the real frame rate.
    pub fn record_present(d: Duration) {
        if !enabled() {
            return;
        }
        PRESENTS.with(|p| {
            let mut v = p.borrow_mut();
            WINDOW_START.with(|w| {
                if w.borrow().is_none() {
                    *w.borrow_mut() = Some(std::time::Instant::now());
                }
            });
            v.push(d);
        });
    }

    /// Record one frame's breakdown; flush aggregate stats every
    /// [`FLUSH_EVERY`] samples. No-op (one atomic load) when the env
    /// var is unset, so leaving the calls in the hot path is free.
    pub fn record(s: Sample) {
        if !enabled() {
            return;
        }
        SAMPLES.with(|cell| {
            let mut v = cell.borrow_mut();
            v.push(s);
            if v.len() < FLUSH_EVERY {
                return;
            }
            let n = v.len();
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let pct = |sel: &dyn Fn(&Sample) -> Duration, q: usize| {
                let mut xs: Vec<Duration> = v.iter().map(sel).collect();
                xs.sort_unstable();
                ms(xs[(n * q / 100).min(n - 1)])
            };
            let mean = |sel: &dyn Fn(&Sample) -> Duration| ms(v.iter().map(sel).sum::<Duration>() / n as u32);
            let mean_total = v.iter().map(|s| s.total).sum::<Duration>() / n as u32;
            let ceiling = if mean_total.as_secs_f64() > 0.0 {
                1.0 / mean_total.as_secs_f64()
            } else {
                f64::INFINITY
            };
            // Actual GPU present rate over this window — the number
            // that tracks watts. With the sustained-output frame cap,
            // an animated TUI should show ~30 fps here even though the
            // 16 ms pump ticks at 60.
            let (present_fps, present_mean_ms, present_n) = PRESENTS.with(|p| {
                let pv = std::mem::take(&mut *p.borrow_mut());
                let elapsed = WINDOW_START.with(|w| w.borrow_mut().take().map(|t| t.elapsed()));
                let cnt = pv.len();
                let fps = match elapsed {
                    Some(e) if e.as_secs_f64() > 0.0 => cnt as f64 / e.as_secs_f64(),
                    _ => 0.0,
                };
                let mean = if cnt > 0 {
                    ms(pv.iter().sum::<Duration>() / cnt as u32)
                } else {
                    0.0
                };
                (fps, mean, cnt)
            });
            eprintln!(
                "paint: {n} prepaints · total mean {:.2} p50 {:.2} p99 {:.2}ms · \
                 lock+scan mean {:.2} p99 {:.2}ms · shape mean {:.2} p99 {:.2}ms · ceiling {ceiling:.0} fps \
                 || presents {present_n} @ {present_fps:.0} fps (mean {present_mean_ms:.2}ms)",
                mean(&|s| s.total),
                pct(&|s| s.total, 50),
                pct(&|s| s.total, 99),
                mean(&|s| s.phase1),
                pct(&|s| s.phase1, 99),
                mean(&|s| s.phase2),
                pct(&|s| s.phase2, 99),
            );
            v.clear();
        });
    }
}

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};

use crate::terminal_utils::{hsla_eq, is_default_bg, is_default_fg, keystroke_to_bytes};
use crate::theme::{self, ThemeName};
use crate::{FontConfig, detect_urls, file_path_for_open};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use gpui::{
    App, AsyncApp, Bounds, ClipboardItem, ContentMask, Context, Corners, Edges, Element, ElementId, FocusHandle,
    Focusable, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement,
    Pixels, Render, Rgba, ScrollWheelEvent, ShapedLine, Size, StrikethroughStyle, Style, Styled, TextRun, TouchPhase,
    UnderlineStyle, WeakEntity, Window, div, fill, font, point, px, relative, size,
};
// Color / NamedColor are no longer referenced directly — the SGR emit
// loop lives in `crate::term_export` and the tests below import what
// they need from there.

const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;
const SCROLLBAR_WIDTH: f32 = 8.0;
/// Minimum spacing between grid reflows during a resize storm. A live
/// window drag crosses a cell boundary on nearly every frame, and each
/// crossing used to reflow the full 10k-line scrollback + SIGWINCH the
/// PTY app, synchronously on the UI thread. The first change applies
/// instantly (a lone maximize keeps its immediate reflow); faster
/// follow-ups park as pending and the repaint pump applies the final
/// size once the drag settles.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone)]
pub struct DetectedUrl {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
    /// `Rc<str>` — the detected-URL list is rebuilt every painted frame
    /// from the per-line cache, so the URL text is shared rather than
    /// re-allocated per frame.
    pub url: std::rc::Rc<str>,
    pub is_file: bool,
}

/// Alacritty calls `send_event(Event::PtyWrite(text))` whenever the
/// VT parser produces a reply that has to travel back into the PTY's
/// stdin — Device Status Report (`ESC[6n`), primary device attributes,
/// window-size queries, color queries, and so on. The default trait
/// impl is a no-op, which silently drops those replies and breaks
/// anything that waits on them (reedline times out on its cursor-
/// position probe, for instance). This proxy holds a slot for the
/// `EventLoopSender` that the caller fills in once `EventLoop::spawn`
/// has handed it back; until then events are buffered into the void,
/// which is fine because no PTY exists to read them yet.
#[derive(Clone, Default)]
struct EventProxy {
    notifier: Arc<std::sync::Mutex<Option<EventLoopSender>>>,
    /// Active theme, so OSC colour queries (see `send_event`) answer with
    /// the palette the tab is actually painted in. Kept in sync by
    /// [`TerminalView::set_theme`]. `Arc<Mutex<_>>` because the proxy is
    /// cloned into the parser thread.
    theme: Arc<std::sync::Mutex<ThemeName>>,
    /// Flipped by `ChildExit` — alacritty's event loop already watches
    /// the PTY child, so the shell's death arrives as an event instead
    /// of the 500 ms `process_alive` `/proc` poll every tab used to run
    /// for its whole life. Shared with [`TerminalView::exited`].
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl EventProxy {
    fn set_notifier(&self, sender: EventLoopSender) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(sender);
        }
    }

    fn set_theme(&self, theme: ThemeName) {
        if let Ok(mut t) = self.theme.lock() {
            *t = theme;
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        let bytes: Vec<u8> = match event {
            AlacrittyEvent::PtyWrite(text) => text.into_bytes(),
            AlacrittyEvent::ChildExit(_) => {
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            // Answer OSC colour queries (OSC 4 palette / 10 fg / 11 bg /
            // 12 cursor). Without a reply the query times out and the app
            // assumes a default (near-black) background — Claude Code then
            // computes its diff highlight colours for that imagined bg, and
            // those clash with our real navy theme (added lines render a
            // blue that nearly matches the background). Replying with the
            // actual palette lets the app blend against the right bg.
            AlacrittyEvent::ColorRequest(index, formatter) => {
                let theme = self.theme.lock().map_or_else(|_| ThemeName::default(), |t| *t);
                formatter(crate::theme::theme(theme).color_index_to_rgb(index)).into_bytes()
            }
            _ => return,
        };
        if let Ok(slot) = self.notifier.lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }
}

use crate::term_export::TermDims;

/// `(start_col, end_col, url, is_file)` — one detected URL in a cached line.
type CachedUrl = (usize, usize, std::rc::Rc<str>, bool);

struct CachedLine {
    text: String,
    /// Shared with the per-frame `TermLine::segments`. Wrapping in `Rc` so
    /// a cache hit costs one atomic bump instead of deep-cloning the whole
    /// `Vec<TermSegment>` (each segment carries a `ShapedLine`).
    segments: std::rc::Rc<Vec<TermSegment>>,
    /// URLs detected in this line, computed once on cache miss and reused
    /// on every subsequent hit so `detect_urls` doesn't re-run every frame.
    /// Like `crate::detect_urls`'s return, with the URL text `Rc`-shared
    /// so the per-frame `DetectedUrl` rebuild is refcount bumps.
    urls: std::rc::Rc<Vec<CachedUrl>>,
    /// Fingerprint of the shaping inputs the plain `text` does NOT capture
    /// (per-run colour + font weight/style + under/strikethrough). The cache
    /// is keyed on [`RawLine::abs_line`]; validating on `text` alone let a
    /// line whose text was unchanged but whose colour changed reuse stale
    /// glyphs shaped in the old colour. See [`line_style_sig`].
    sig: u64,
}

/// Everything [`TerminalView::ensure_spawned`] needs to fork the shell for a
/// tab whose spawn was deferred at startup (skeleton tab). Mirrors the initial
/// spawn inputs; the working size comes from `last_size`/`cell_size`, colours
/// from `colors_enabled`, so only these three per-tab bits are stashed.
struct SpawnRecipe {
    cwd: Option<std::path::PathBuf>,
    extra_env: HashMap<String, String>,
    agent_launch: Option<Vec<String>>,
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<EventProxy>>>,
    /// Channel into the PTY event loop. `None` for a *skeleton* tab whose shell
    /// hasn't been forked yet (deferred at startup for a fast first paint — see
    /// [`Self::ensure_spawned`]). Every send guards on it.
    notifier: Option<EventLoopSender>,
    event_proxy: EventProxy,
    focus: FocusHandle,
    cell_size: Option<Size<Pixels>>,
    last_size: Rc<Cell<Option<(usize, usize)>>>,
    /// Resize parked during a storm: `(cols, lines, requested_at)` —
    /// applied by [`Self::apply_pending_resize`] once stable. See
    /// [`RESIZE_SETTLE`].
    pending_resize: Rc<Cell<Option<(usize, usize, std::time::Instant)>>>,
    /// When the grid was last actually reflowed (leading-edge stamp).
    last_resize_apply: Rc<Cell<Option<std::time::Instant>>>,
    content_origin: Rc<Cell<gpui::Point<Pixels>>>,
    bounds_size: Rc<Cell<Size<Pixels>>>,
    line_cache: Rc<RefCell<HashMap<i32, CachedLine>>>,
    /// Last frame's map, kept as scratch so Phase 2 never allocates a
    /// fresh `HashMap` per frame — the two maps swap roles each paint.
    line_cache_scratch: Rc<RefCell<HashMap<i32, CachedLine>>>,
    /// PTY child pid, or `0` until the shell is spawned (skeleton tab).
    pid: u32,
    /// Deferred-spawn inputs. `Some` for a skeleton tab that hasn't forked its
    /// shell yet; `ensure_spawned` takes it to build the PTY. `None` once spawned.
    spawn_recipe: Option<SpawnRecipe>,
    /// Set by the PTY event-loop thread when the shell dies (see
    /// [`EventProxy::exited`]); read on the UI thread by `has_exited`.
    exited: Arc<std::sync::atomic::AtomicBool>,
    scrollbar_dragging: Rc<Cell<bool>>,
    scroll_acc: Rc<Cell<f32>>,
    pub theme: ThemeName,
    font_config: FontConfig,
    browser: Rc<RefCell<Option<String>>>,
    code_editor: Rc<RefCell<Option<String>>>,
    detected_urls: Rc<RefCell<Vec<DetectedUrl>>>,
    hover_grid: Rc<Cell<Option<(usize, usize)>>>,
    click_origin: Rc<Cell<Option<GridPoint>>>,
    last_input: Rc<Cell<Option<std::time::Instant>>>,
    colors_enabled: Cell<bool>,
    /// When true, every write path (typing, paste, programmatic
    /// `send_input_bytes`) early-returns. The lock state lives on the
    /// view so input is blocked at the chokepoint regardless of
    /// caller — keyboard event handlers, the API drain, the catbus
    /// menu item, etc.
    locked: Rc<Cell<bool>>,
    /// Raw PTY byte ring captured BEFORE alacritty parses anything.
    /// Source of truth for share-link viewer scrollback because
    /// alacritty's grid history is wiped by `\x1b[3J` and doesn't
    /// grow for in-place TUI redraws (Claude Code, htop, less, …).
    /// Survives PTY respawn — the same Arc threads into the next
    /// `PtyTap` so the user can scroll past the restart boundary.
    pty_ring: Arc<std::sync::Mutex<crate::pty_ring::PtyRing>>,
    /// Lock-free copy of the ring's WS-viewer counter, so the renderer
    /// can ask "is anyone watching this tab over the web/remote?" each
    /// frame without taking the ring lock. Bumped by `api_ws::run_pump`.
    viewers: Arc<std::sync::atomic::AtomicUsize>,
    /// Lock-free mirror of the ring's `total_len` (see
    /// [`crate::pty_ring::PtyRing::total_len_handle`]) — the repaint pump
    /// and the persist tick probe "did output arrive" through it without
    /// touching the ring mutex.
    ring_len_mirror: Arc<std::sync::atomic::AtomicU64>,
    /// When true the PTY is (re)spawned inside a bubblewrap sandbox with
    /// its own empty network namespace — the tab has no internet. Set via
    /// [`Self::set_net_disabled`]; the caller then respawns so the change
    /// takes effect (the running shell can't be re-jailed in place). The
    /// fresh-tab spawn always starts net-on; only `respawn` consults this,
    /// so a restored net-off tab is respawned by `app.rs`. `Cell` so the
    /// flag flips through a shared `&self` view handle like `colors_enabled`.
    net_disabled: Cell<bool>,
    /// Previous frame's per-row `RawLine`s, reused for un-damaged rows
    /// (Ghostty `rebuildCells` pattern). Alacritty's `Term::damage()`
    /// exposes which screen rows changed since the last call; rows
    /// outside that set keep their cached `RawLine` and the Phase 1
    /// cell-scan is skipped entirely for them.
    ///
    /// Cache validity / row realignment is decided by
    /// [`CachedFrame::shift_for`]: a resize or any output-while-the-
    /// window-moved discards the rows, a pure user scroll *shifts* them
    /// (the content is unchanged, only the viewport window moved), and
    /// a stationary window reuses them index-for-index under damage.
    /// `history_size` matters for a *live*-screen scroll: pushing a
    /// line into scrollback shifts every visible row up while
    /// `display_offset` stays 0, so index-keyed reuse without the shift
    /// check serves stale rows (e.g. an old inverse-red diff line
    /// bleeding red under the next thing drawn there).
    prev_frame: Rc<RefCell<Option<CachedFrame>>>,
    /// Last fully-built prepaint output. When the parser thread holds
    /// the `Term` lock at prepaint time, the UI thread reuses this
    /// instead of blocking — see the `try_lock_unfair` path in
    /// `prepaint`. The next 16 ms tick retries with fresh grid data.
    last_prepaint: Rc<RefCell<Option<TermPrepaint>>>,
    /// Wall-clock of the last time this view was actually rendered (i.e.
    /// mounted as the visible/active tab). `render` stamps it; the repaint
    /// pump reads it to decide whether it's the foreground view. Only the
    /// active tab is put in the element tree, so a background tab's stamp
    /// goes stale and its pump stops scheduling frames — without this, every
    /// one of N background tabs streaming agent output would `notify()` at
    /// ~30 fps and each schedules a full window repaint of the *active*
    /// terminal, starving keystroke handling (the "typing is laggy" bug).
    /// `None` until first paint, so restored-but-unopened tabs never pump.
    last_render: Rc<Cell<Option<std::time::Instant>>>,
}

/// Cached Phase 1 output from the previous paint. See [`TerminalView::prev_frame`].
///
/// `lines` is stored as `Vec<Option<RawLine>>` (not `Vec<RawLine>`) so
/// the per-frame `working` Vec can be moved in / out without
/// allocating a fresh Vec each paint to wrap entries in `Some`. The
/// Vec's capacity is reused across frames — only damaged-row slots
/// are taken + replaced, the rest stay put. Ghostty's
/// `Contents::clearRetainingCapacity` zero-alloc pattern, mapped to
/// Rust's `Vec` storage reuse.
struct CachedFrame {
    display_offset: i32,
    visible_cols: usize,
    visible_lines: usize,
    /// Scrollback length at build time. A change means the live screen
    /// scrolled (rows shifted up), so the index-keyed rows only stay
    /// valid through the shift arithmetic in [`Self::shift_for`].
    history_size: usize,
    /// PTY ring byte counter at build time. Bytes are counted into the
    /// ring BEFORE alacritty parses them, so an unchanged counter means
    /// the grid *content* is unchanged and only the viewport window can
    /// have moved (a `scroll_display`) — the signal that makes shifted
    /// reuse safe for user scrolling.
    ring_len: u64,
    /// `Rc` so handing the rows to Phase 2 each frame is a refcount
    /// bump per row — the old `RawLine` deep clone (text + segments +
    /// runs for EVERY visible row) ran under the Term lock every paint.
    lines: Vec<Option<Rc<RawLine>>>,
}

impl CachedFrame {
    /// How the current viewport maps onto this cached frame's rows, or
    /// `None` when the rows must be rebuilt from scratch.
    ///
    /// `Some(0)`: same window (offset AND scrollback length unchanged) —
    /// rows are index-stable, caller applies damage as usual.
    ///
    /// `Some(shift)`: the window moved over UNCHANGED content (the ring
    /// counter proves no PTY bytes arrived since the build): slot `i`
    /// now shows what old slot `i + shift` held. This is every wheel /
    /// scrollbar step — previously a full rescan + reshape of all rows
    /// per step, because alacritty marks the whole screen damaged on
    /// any `scroll_display`.
    ///
    /// `None`: geometry changed, or output arrived while the window
    /// moved (streaming, `\x1b[3J`, scroll-during-flood). The old
    /// stale-bg-bleed guard lives here: a live-screen scroll grows
    /// `history_size` while `display_offset` stays 0, and index-keyed
    /// reuse without the shift served stale inverse-video rows.
    fn shift_for(
        &self,
        display_offset: i32,
        visible_cols: usize,
        visible_lines: usize,
        history_size: usize,
        ring_len: u64,
    ) -> Option<i32> {
        if self.visible_cols != visible_cols || self.visible_lines != visible_lines {
            return None;
        }
        if self.display_offset == display_offset && self.history_size == history_size {
            return Some(0);
        }
        if ring_len != self.ring_len {
            return None;
        }
        // Content-stable line coordinate: `history_size - display_offset`
        // is the scrollback line shown in viewport slot 0. Same content,
        // different window → the slot delta is the difference of bases.
        let base = history_size as i64 - i64::from(display_offset);
        let prev_base = self.history_size as i64 - i64::from(self.display_offset);
        i32::try_from(base - prev_base).ok()
    }
}

/// Realign cached rows after a pure scroll: slot `i` takes the row that
/// sat at slot `i + shift`; slots whose source falls outside the old
/// window become `None` (rebuilt by the cell scan). In-place rotation —
/// no per-scroll-step Vec allocation.
fn shift_rows<T>(rows: &mut [Option<T>], shift: i32) {
    let s = rows.len().min(shift.unsigned_abs() as usize);
    if s == rows.len() {
        for slot in rows.iter_mut() {
            *slot = None;
        }
    } else if shift > 0 {
        rows.rotate_left(s);
        let start = rows.len() - s;
        for slot in &mut rows[start..] {
            *slot = None;
        }
    } else {
        rows.rotate_right(s);
        for slot in &mut rows[..s] {
            *slot = None;
        }
    }
}

/// Encode a scroll-wheel gesture as the byte sequence a TUI listening
/// in `ALTERNATE_SCROLL` mode (vim/less/htop on alt-screen) wants to see.
/// One `\x1bOA` per line for scroll-back, one `\x1bOB` per line for
/// scroll-forward — matches Zed's `alt_scroll()`.
///
/// Positive `lines` ⇒ user wants OLDER content ⇒ up-arrow.
fn alt_scroll_bytes(lines: i32) -> Vec<u8> {
    let cmd = if lines > 0 { b'A' } else { b'B' };
    let n = lines.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(n * 3);
    for _ in 0..n {
        out.extend_from_slice(&[0x1b, b'O', cmd]);
    }
    out
}

fn pty_env(colors_enabled: bool) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if colors_enabled {
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
    } else {
        env.insert("TERM".into(), "dumb".into());
    }
    // Force the telemetry / feedback-survey opt-out onto every tab.
    crate::apply_telemetry_disable_env(&mut env);
    env
}

impl TerminalView {
    /// Bare-spawn helper, used by the unit tests. Production spawn
    /// sites all go through `new_with_colors_and_env` because they
    /// need to inject per-tab API env vars.
    #[cfg(test)]
    pub fn new(
        cwd: Option<&Path>,
        font_config: FontConfig,
        browser: Rc<RefCell<Option<String>>>,
        code_editor: Rc<RefCell<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_colors_and_env(
            cwd,
            font_config,
            browser,
            code_editor,
            true,
            HashMap::new(),
            None,
            None,
            false, // tests want a live shell immediately
            window,
            cx,
        )
    }

    /// `new` plus per-tab env-var injection (`_TAB_ID`,
    /// `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN`) so tools
    /// running inside the tab can call back into the local API.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_colors_and_env(
        cwd: Option<&Path>,
        font_config: FontConfig,
        browser: Rc<RefCell<Option<String>>>,
        code_editor: Rc<RefCell<Option<String>>>,
        initial_colors_enabled: bool,
        extra_env: HashMap<String, String>,
        // Restored agent tab → extra shell args to launch the agent directly
        // (`… -i -c 'exec claude --resume <id>'`, see `agent_launch_shell_suffix`)
        // so the tab's process IS claude. Only honoured in cleared-env mode.
        agent_launch: Option<Vec<String>>,
        // Grid size to spawn the PTY at (cols, lines, cell). When `None` the
        // PTY starts at the 80×24 fallback and resizes on first paint. Passing
        // the real window-derived size means even a never-shown tab's PTY (and
        // its remote xterm.js viewer) is correctly sized from birth.
        initial_grid: Option<(usize, usize, Size<Pixels>)>,
        // Skeleton tab: build the view but DON'T fork the shell yet. The active
        // tab spawns eagerly (fast first paint); background tabs pass `true` and
        // are forked later by [`Self::ensure_spawned`] (the app's boot loader).
        defer_spawn: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (init_cols, init_lines) = initial_grid.map_or((INITIAL_COLS, INITIAL_LINES), |(c, l, _)| (c, l));
        let init_cell = initial_grid.map(|(_, _, cell)| cell);
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let proxy = EventProxy::default();
        let term = Term::new(
            config,
            &TermDims {
                columns: init_cols,
                screen_lines: init_lines,
            },
            proxy.clone(),
        );
        let term = Arc::new(FairMutex::new(term));
        let pty_ring = Arc::new(std::sync::Mutex::new(crate::pty_ring::PtyRing::default()));
        let viewers = pty_ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .viewers_handle();

        let focus = cx.focus_handle();

        // Repaint pump.
        //
        // Alacritty's PTY reader doesn't notify the gpui view when it
        // writes a byte into the grid, so we poll. Up to one poll
        // interval of latency stacks on top of every keystroke before
        // the echo paints. 16 ms = a single 60 Hz frame; the previous
        // 33 ms was a perceptible "typing feels sluggish" delay.
        //
        // Idle tabs no longer pay for the polling: we read the PTY
        // ring's monotonic byte counter and skip `cx.notify()` when
        // it hasn't moved. The ring counter is updated under a small
        // dedicated mutex (Mutex<PtyRing>) that the alacritty reader
        // holds for microseconds, so the check is cheap and lock-
        // contention-free.
        //
        // The scrolled-up throttle (every 6th tick when the user is
        // browsing scrollback) stays in place — even when the grid
        // hasn't changed, the cursor blink + selection-highlight need
        // periodic redraws so the user can SEE they're paused.
        //
        // Sustained-output frame cap. A single change (a keystroke's
        // echo) repaints on the very next 16 ms tick — full
        // responsiveness. But when output keeps arriving every tick
        // (an animated TUI's redraw loop, a flood, the piu-piu menu's
        // twinkling starfield), painting all 60 fps is pure power
        // burn — the eye can't tell 30 from 60 fps of terminal
        // content, and each present wakes the GPU. So under sustained
        // dirtiness we paint every OTHER dirty tick (~30 fps). The
        // FIRST dirty tick of a burst still paints immediately (odd
        // count), and the settle-frame (dirty → clean transition) is
        // always painted so the final state isn't a tick stale.
        let tick = Rc::new(Cell::new(0u32));
        let tick_clone = tick;
        let last_ring_len = Rc::new(Cell::new(0u64));
        let dirty_streak = Rc::new(Cell::new(0u32));
        let last_render = Rc::new(Cell::new(None::<std::time::Instant>));
        let last_render_pump = last_render.clone();
        let ring_len_mirror = pty_ring.lock().map_or_else(
            |_| Arc::new(std::sync::atomic::AtomicU64::new(0)),
            |r| r.total_len_handle(),
        );
        let ring_len_mirror_pump = ring_len_mirror.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // Poll fast (one 60 Hz frame) only while this tab is the foreground
            // one; a background/asleep tab backs off to `IDLE`, and one nobody
            // has looked at for a minute parks at `DEEP`, so N tabs don't each
            // wake the main thread 60×/s just to read a ring counter they
            // won't paint. A tab switch re-stamps `last_render` and renders
            // immediately; the pump is back to `FAST` within one parked poll.
            const FAST: Duration = Duration::from_millis(16);
            const IDLE: Duration = Duration::from_millis(250);
            const DEEP: Duration = Duration::from_secs(1);
            // How long after its last paint a view still counts as the
            // foreground tab (a few cursor-blink heartbeats of slack), and
            // how long until a background tab is considered parked.
            const FRESH: Duration = Duration::from_secs(2);
            const PARKED: Duration = Duration::from_mins(1);
            let ring_len_mirror = ring_len_mirror_pump;
            let mut interval = FAST;
            let mut parked = false;
            loop {
                cx.background_executor().timer(interval).await;
                if this.upgrade().is_none() {
                    break;
                }
                let n = tick_clone.get().wrapping_add(1);
                tick_clone.set(n);
                // Lock-free ring probe — the mutex is contended by the PTY
                // reader thread during floods, and N background pumps have
                // no business queueing on it.
                let ring_len = ring_len_mirror.load(std::sync::atomic::Ordering::Relaxed);
                let grid_dirty = ring_len != last_ring_len.get();
                last_ring_len.set(ring_len);
                // Foreground gate: only the visible/active tab is mounted,
                // so only it is ever painted (`render` stamps `last_render`).
                // A background tab's stamp goes stale within FRESH; skip the
                // entity update entirely so N streaming agents can't drive
                // N full-window repaints per frame. The active tab keeps
                // itself fresh via its own repaints (keystroke echo + the
                // 500 ms cursor-blink heartbeat below), and a tab switch
                // re-renders the new active view, re-stamping it at once.
                let render_age = last_render_pump.get().map(|t| t.elapsed());
                if render_age.is_none_or(|a| a >= FRESH) {
                    if render_age.is_some_and(|a| a < PARKED) {
                        interval = IDLE;
                    } else {
                        interval = DEEP;
                        // Park transition: nobody has looked at this tab in
                        // a minute — free its shaped-glyph / frame caches.
                        // Rebuilt in one frame when it's next shown, which
                        // a tab switch pays anyway.
                        if !parked {
                            parked = true;
                            let _ = this.update(cx, |view, _| view.release_render_caches());
                        }
                    }
                    continue;
                }
                interval = FAST;
                parked = false;
                let Ok(()) = this.update(cx, |view, cx: &mut Context<Self>| {
                    // A resize parked by the prepaint rate-limit applies
                    // here once the drag settles (see RESIZE_SETTLE).
                    if view.apply_pending_resize() {
                        cx.notify();
                    }
                    if grid_dirty {
                        let streak = dirty_streak.get().wrapping_add(1);
                        dirty_streak.set(streak);
                        // Paint on odd ticks of a sustained streak →
                        // 30 fps cap; the first tick (streak 1) is odd
                        // so a lone keystroke paints immediately.
                        if streak % 2 == 1 {
                            cx.notify();
                        }
                    } else {
                        // Output just settled — paint the final frame
                        // once if we were mid-streak (so a skipped even
                        // tick doesn't leave the screen a frame behind).
                        if dirty_streak.get() > 0 {
                            cx.notify();
                        }
                        dirty_streak.set(0);
                        // Read the scroll offset only on this quiet path, and
                        // without blocking: a plain `lock()` here ran on EVERY
                        // 16 ms tick, and during a flood the parser thread
                        // holds the Term lock for whole parse batches — the
                        // same main-thread stall the prepaint's try-lock path
                        // exists to avoid. On contention assume "not
                        // scrolled"; the ring counter is moving then anyway,
                        // so this branch isn't the one painting.
                        let scrolled = view
                            .term
                            .try_lock_unfair()
                            .is_some_and(|t| t.grid().display_offset() > 0);
                        if scrolled && n.is_multiple_of(12) {
                            // Scrolled-up: paint twice a second so the
                            // cursor blink stays alive without burning CPU.
                            cx.notify();
                        } else if n.is_multiple_of(30) {
                            // Idle attached: ~once every 500 ms for the
                            // cursor blink at the bottom.
                            cx.notify();
                        }
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        // Shell death arrives as `ChildExit` on this flag (see
        // `EventProxy::exited`) — no per-tab watcher loop needed.
        let exited = proxy.exited.clone();

        let recipe = SpawnRecipe {
            cwd: cwd.map(std::path::Path::to_path_buf),
            extra_env,
            agent_launch,
        };

        let mut me = Self {
            term,
            // Forked lazily below (or by the boot loader) — see `ensure_spawned`.
            notifier: None,
            event_proxy: proxy,
            focus,
            // Seed from `initial_grid` so a tab spawned at a known size doesn't
            // re-measure the cell or thrash a resize on its first paint.
            cell_size: init_cell,
            last_size: Rc::new(Cell::new(initial_grid.map(|(c, l, _)| (c, l)))),
            pending_resize: Rc::new(Cell::new(None)),
            last_resize_apply: Rc::new(Cell::new(None)),
            content_origin: Rc::new(Cell::new(point(px(0.0), px(0.0)))),
            bounds_size: Rc::new(Cell::new(size(px(0.0), px(0.0)))),
            line_cache: Rc::new(RefCell::new(HashMap::new())),
            line_cache_scratch: Rc::new(RefCell::new(HashMap::new())),
            pid: 0,
            spawn_recipe: Some(recipe),
            exited,
            scrollbar_dragging: Rc::new(Cell::new(false)),
            scroll_acc: Rc::new(Cell::new(0.0)),
            theme: ThemeName::default(),
            font_config,
            browser,
            code_editor,
            detected_urls: Rc::new(RefCell::new(Vec::new())),
            hover_grid: Rc::new(Cell::new(None)),
            click_origin: Rc::new(Cell::new(None)),
            last_input: Rc::new(Cell::new(None)),
            colors_enabled: Cell::new(initial_colors_enabled),
            locked: Rc::new(Cell::new(false)),
            pty_ring,
            viewers,
            ring_len_mirror,
            net_disabled: Cell::new(false),
            prev_frame: Rc::new(RefCell::new(None)),
            last_prepaint: Rc::new(RefCell::new(None)),
            last_render,
        };
        // The active tab forks its shell now (so the first frame is live); a
        // deferred skeleton waits for `ensure_spawned` (the app's boot loader
        // or the switch/render that first shows it).
        if !defer_spawn {
            me.ensure_spawned();
        }
        me
    }

    /// Fork the shell + start the PTY event loop for a tab whose spawn was
    /// deferred (skeleton). No-op once spawned. Mirrors the initial-spawn path,
    /// pulling the working size from `last_size`/`cell_size` and the shell/env
    /// from the stashed [`SpawnRecipe`]. Called eagerly for the active tab, and
    /// in the background for the rest so restored agents come back online.
    pub fn ensure_spawned(&mut self) {
        let Some(recipe) = self.spawn_recipe.take() else {
            return;
        };
        // A shell tab prints its prompt as its very first output. On a fresh
        // PTY that can glue onto whatever's already on the grid — a restored
        // prompt, or bash's own SIGWINCH re-display after the initial resize —
        // leaving two prompts on one line (`…$ …$`). Emit a leading CRLF (below,
        // once the term exists) so the first prompt lands on its own clean line.
        // Agent tabs exec `claude`, which clears + redraws its own screen, so
        // they don't need — and shouldn't get — the extra line.
        let is_shell = recipe.agent_launch.is_none();
        let (cols, lines) = self.last_size.get().unwrap_or((INITIAL_COLS, INITIAL_LINES));
        let cell = self.cell_size.unwrap_or(Size {
            width: px(8.4),
            height: px(19.6),
        });
        let ws = WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: f32::from(cell.width) as u16,
            cell_height: f32::from(cell.height) as u16,
        };
        let colors = self.colors_enabled.get();
        let opts = if crate::clear_env() {
            // Cleared-env mode: exec via `env -i` so the shell inherits only the
            // curated minimal allowlist. `login = true` sources the profile.
            let min_env = crate::minimal_pty_env(colors, crate::clear_env_user_vars(), &recipe.extra_env);
            let (prog, mut args) = crate::clear_env_shell_command(&crate::clear_env_shell_path(), true, &min_env);
            if let Some(suffix) = recipe.agent_launch {
                args.extend(suffix);
            }
            tty::Options {
                shell: Some(tty::Shell::new(prog, args)),
                working_directory: recipe.cwd,
                env: HashMap::new(),
                ..Default::default()
            }
        } else {
            let mut env = pty_env(colors);
            env.extend(recipe.extra_env);
            tty::Options {
                working_directory: recipe.cwd,
                env,
                ..Default::default()
            }
        };
        let pty = match tty::new(&opts, ws, 0) {
            Ok(pty) => pty,
            Err(e) => {
                // Can't fork the shell (out of fds/pids, etc.). Don't crash the
                // whole app — mark this one tab dead so it renders as exited.
                error!("failed to create PTY: {e}");
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        // ConPTY's Pty doesn't expose the child like the Unix one; pid feeds
        // /proc cwd + catbus detection (Linux-only), so a 0 sentinel is fine.
        #[cfg(unix)]
        let pid = pty.child().id();
        #[cfg(windows)]
        let pid = 0u32;
        let pty = crate::pty_ring::PtyTap::new(pty, self.pty_ring.clone());
        let el = match EventLoop::new(self.term.clone(), self.event_proxy.clone(), pty, false, false) {
            Ok(el) => el,
            Err(e) => {
                error!("failed to create PTY event loop: {e}");
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let notifier = el.channel();
        self.event_proxy.set_notifier(notifier.clone());
        el.spawn();
        self.notifier = Some(notifier);
        self.pid = pid;
        // A shell tab prints its prompt as its first output, which on a fresh /
        // just-resized PTY can land glued to a reprint of itself (`…$ …$` on one
        // line, from bash's SIGWINCH re-display). Send one Enter so bash emits a
        // clean CRLF + a fresh prompt on its own line. RAW PTY write — NOT via
        // `send_input`, whose keystroke bookkeeping (last_input stamp, predictive
        // echo) must not fire for a synthetic newline. Agent tabs exec `claude`
        // (clears + draws its own screen), so skip them.
        if is_shell && let Some(n) = &self.notifier {
            let _ = n.send(Msg::Input(b"\r".to_vec().into()));
        }
        self.exited.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the shell has been forked yet (`false` for a skeleton tab).
    #[must_use]
    pub const fn is_spawned(&self) -> bool {
        self.spawn_recipe.is_none()
    }

    /// Clone of the per-tab PTY ring's Arc. Lets the snapshot
    /// pipeline expose the ring to the API layer without giving the
    /// snapshot mutable access to the rest of the view.
    #[must_use]
    pub fn pty_ring(&self) -> Arc<std::sync::Mutex<crate::pty_ring::PtyRing>> {
        self.pty_ring.clone()
    }

    /// How many WS viewers (browser share-link / `remote attach`) are
    /// currently watching this tab. Lock-free.
    #[must_use]
    pub fn viewer_count(&self) -> usize {
        self.viewers.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total bytes ever written through the tab's PTY ring — the
    /// universal "did output arrive" dirtiness key. Lock-free.
    #[must_use]
    pub fn ring_len(&self) -> u64 {
        self.ring_len_mirror.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub const fn colors_enabled(&self) -> bool {
        self.colors_enabled.get()
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn set_colors_enabled(&self, enabled: bool) {
        self.colors_enabled.set(enabled);
    }

    /// Set the active theme. Updates the field the renderer reads AND the
    /// copy the event proxy uses to answer OSC colour queries, so a TUI
    /// querying the terminal background always sees the current palette.
    /// Use this instead of writing `view.theme` directly.
    pub fn set_theme(&mut self, theme: ThemeName) {
        self.theme = theme;
        self.event_proxy.set_theme(theme);
    }

    pub fn restore_output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
        let mut term = self.term.lock();
        for line in text.lines() {
            parser.advance(&mut *term, line.as_bytes());
            parser.advance(&mut *term, b"\r\n");
        }
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Drop the render caches (previous frame's rows, shaped-glyph cache,
    /// stashed prepaint, detected URLs). Called when the tab leaves the
    /// foreground — a parked tab can pin hundreds of KB of shaped glyphs
    /// it cannot paint; the first frame after re-activation rebuilds them
    /// from the grid in one pass, which a tab switch pays anyway.
    pub fn release_render_caches(&self) {
        self.prev_frame.borrow_mut().take();
        self.last_prepaint.borrow_mut().take();
        let mut cache = self.line_cache.borrow_mut();
        cache.clear();
        cache.shrink_to_fit();
        drop(cache);
        let mut scratch = self.line_cache_scratch.borrow_mut();
        scratch.clear();
        scratch.shrink_to_fit();
        drop(scratch);
        let mut urls = self.detected_urls.borrow_mut();
        urls.clear();
        urls.shrink_to_fit();
    }

    pub fn last_input_time(&self) -> Option<std::time::Instant> {
        self.last_input.get()
    }

    fn visible_lines(&self) -> usize {
        self.cell_size
            .map_or(25, |c| {
                (f32::from(self.bounds_size.get().height) / f32::from(c.height)) as usize
            })
            .max(1)
    }

    /// Whether this tab is currently running with no internet (its PTY is
    /// inside a bubblewrap network-isolated sandbox).
    pub const fn net_disabled(&self) -> bool {
        self.net_disabled.get()
    }

    /// Record the desired internet on/off state. Takes effect on the next
    /// [`Self::respawn`] — the caller respawns (history-preserving from the
    /// GUI) so the change applies; the running shell can't be re-jailed.
    pub fn set_net_disabled(&self, disabled: bool) {
        self.net_disabled.set(disabled);
    }

    pub fn respawn(&mut self, cwd: Option<&Path>) {
        if let Some(n) = self.notifier.as_ref() {
            let _ = n.send(Msg::Shutdown);
        }
        // A respawn always ends up with a live process, so this view is no
        // longer a skeleton even if it was one (respawn builds its own opts).
        self.spawn_recipe = None;

        let (cols, lines) = self.last_size.get().unwrap_or((INITIAL_COLS, INITIAL_LINES));
        let cell = self.cell_size.unwrap_or(Size {
            width: px(8.4),
            height: px(19.6),
        });

        let ws = WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: f32::from(cell.width) as u16,
            cell_height: f32::from(cell.height) as u16,
        };

        let opts = if crate::clear_env() {
            // Respawn carries no per-tab API extras (same as the
            // inheriting branch below), so the cleared env is just the
            // minimal allowlist + colours + telemetry opt-out.
            let min_env =
                crate::minimal_pty_env(self.colors_enabled.get(), crate::clear_env_user_vars(), &HashMap::new());
            let (prog, args) = crate::clear_env_shell_command(&crate::clear_env_shell_path(), true, &min_env);
            let (prog, args) = if self.net_disabled.get() {
                crate::no_internet_command(&prog, &args)
            } else {
                (prog, args)
            };
            tty::Options {
                shell: Some(tty::Shell::new(prog, args)),
                working_directory: cwd.map(std::path::Path::to_path_buf),
                env: HashMap::new(),
                ..Default::default()
            }
        } else if self.net_disabled.get() {
            // Inheriting env, but net-off: alacritty's implicit default
            // shell can't be wrapped, so spawn the login shell explicitly
            // inside bubblewrap. bwrap inherits `env` and passes it to the
            // child (no --clearenv), so the colour/telemetry vars survive.
            let (prog, args) = crate::no_internet_command(&crate::clear_env_shell_path(), &["-l".to_string()]);
            tty::Options {
                shell: Some(tty::Shell::new(prog, args)),
                working_directory: cwd.map(std::path::Path::to_path_buf),
                env: pty_env(self.colors_enabled.get()),
                ..Default::default()
            }
        } else {
            tty::Options {
                working_directory: cwd.map(std::path::Path::to_path_buf),
                env: pty_env(self.colors_enabled.get()),
                ..Default::default()
            }
        };
        let pty = match tty::new(&opts, ws, 0) {
            Ok(pty) => pty,
            Err(e) => {
                error!("failed to re-create PTY on respawn: {e}");
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        // ConPTY's Pty doesn't expose the child the way the Unix one does.
        // The PID feeds /proc cwd + catbus detection — both Linux-only, so
        // a 0 sentinel is fine on Windows.
        #[cfg(unix)]
        let pid = pty.child().id();
        #[cfg(windows)]
        let pid = 0u32;

        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);

        // Carry the same Arc into the new tap so the viewer's
        // scrollback survives a PTY respawn (shell exited, user
        // hit Enter to relaunch). Bytes from the old process stay
        // in the ring; new bytes append.
        let pty = crate::pty_ring::PtyTap::new(pty, self.pty_ring.clone());
        let el = match EventLoop::new(self.term.clone(), self.event_proxy.clone(), pty, false, false) {
            Ok(el) => el,
            Err(e) => {
                error!("failed to create PTY event loop on respawn: {e}");
                self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let notifier = el.channel();
        self.event_proxy.set_notifier(notifier.clone());
        el.spawn();
        self.notifier = Some(notifier);

        self.pid = pid;
        // Respawn keeps the old grid, so the re-forked shell's prompt would glue
        // onto it — same clean-line fix as `ensure_spawned` (raw PTY write, not a
        // keystroke). Respawn always forks a shell.
        if let Some(n) = &self.notifier {
            let _ = n.send(Msg::Input(b"\r".to_vec().into()));
        }
        self.exited.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn shutdown(&self) {
        if let Some(n) = self.notifier.as_ref() {
            let _ = n.send(Msg::Shutdown);
        }
    }

    fn send_input(&self, bytes: Vec<u8>) {
        if self.locked.get() {
            return;
        }
        self.last_input.set(Some(std::time::Instant::now()));
        // T1 — instrumentation: bytes are about to leave our process
        // toward the PTY notifier. Together with T0 (keystroke entry)
        // this isolates time spent in our pre-send logic
        // (scroll-to-bottom, mode lookup, etc).
        let preview_len = bytes.len().min(16);
        trace!(
            target: crate::INPUT_TRACE_TARGET,
            "T1 send_input bytes={} preview={:?}",
            bytes.len(),
            std::str::from_utf8(&bytes[..preview_len]).unwrap_or("<non-utf8>"),
        );
        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);
        if let Some(n) = self.notifier.as_ref() {
            let _ = n.send(Msg::Input(bytes.into()));
        }
    }

    pub fn send_input_bytes(&self, bytes: Vec<u8>) {
        self.send_input(bytes);
    }

    // Lock-state read accessor — currently used by the right-click
    // menu render to label the toggle, and reserved for any future
    // call site that needs to peek at the gate (paste, hotkey
    // dispatchers). Keep it even if the menu is the only caller.
    #[allow(dead_code)]
    pub fn is_locked(&self) -> bool {
        self.locked.get()
    }

    /// Internal lock gate for local typing + paste. Driven by the
    /// per-tick mirror in `AppState::persist()`, which pushes
    /// `Tab::effective_locked()` here on every tick. Do not call
    /// directly from gate sites — the mirror is the single funnel
    /// so manual lock toggles AND off-hours schedule transitions
    /// both reach the view without a dedicated push for each.
    pub fn set_locked(&self, value: bool) {
        self.locked.set(value);
    }

    pub fn reset_terminal(&self) {
        let reset = concat!(
            "\x1b[?1049l", // exit alternate screen
            "\x1b[0m",     // reset all SGR attributes (colors/styles)
            "\x1b[?25h",   // show cursor
            "\x1b[?1l",    // reset cursor keys to normal mode
            "\x1b[?7h",    // enable auto-wrap
            "\x1b[?2004h", // enable bracketed paste
            "\x1b(B",      // reset charset to ASCII
        );
        let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
        let mut term = self.term.lock();
        parser.advance(&mut *term, reset.as_bytes());
        term.grid_mut().scroll_display(Scroll::Bottom);
    }

    /// Turn whatever's on the clipboard (text or image) into a string
    /// the shell can usefully receive. Image entries are written to a
    /// fresh file under `$TMPDIR/tab-atelier-paste-…` and the path is
    /// returned — interactive TUIs (Claude Code, claude-cli, image
    /// viewers) accept a path; plain shells still get something
    /// reasonable. Returns `None` when the clipboard is empty.
    pub fn clipboard_to_paste_text(item: &ClipboardItem) -> Option<String> {
        use gpui::ClipboardEntry;
        // Prefer text when present — covers the "copied a URL from a
        // browser tab" case where Wayland/X11 ships both text and
        // image previews together.
        if let Some(s) = item.text() {
            return Some(s);
        }
        for entry in item.entries() {
            if let ClipboardEntry::Image(img) = entry {
                let ext = match img.format() {
                    gpui::ImageFormat::Png => "png",
                    gpui::ImageFormat::Jpeg => "jpg",
                    gpui::ImageFormat::Webp => "webp",
                    gpui::ImageFormat::Gif => "gif",
                    gpui::ImageFormat::Bmp => "bmp",
                    gpui::ImageFormat::Tiff => "tiff",
                    gpui::ImageFormat::Svg => "svg",
                };
                // Unique per ms + counter so successive pastes don't
                // collide; keeping the files under one prefix makes
                // cleanup easy (`rm /tmp/tab-atelier-paste-*`).
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                let dir = std::env::temp_dir();
                let path = dir.join(format!("tab-atelier-paste-{ts}.{ext}"));
                if std::fs::write(&path, img.bytes()).is_ok() {
                    return Some(path.to_string_lossy().into_owned());
                }
            }
        }
        None
    }

    pub fn send_clipboard(&self, text: &str) {
        if self.locked.get() {
            return;
        }
        self.last_input.set(Some(std::time::Instant::now()));
        let mut term = self.term.lock();
        let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
        term.grid_mut().scroll_display(Scroll::Bottom);
        drop(term);
        let payload = if bracketed {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        if let Some(n) = self.notifier.as_ref() {
            let _ = n.send(Msg::Input(payload.into_bytes().into()));
        }
    }

    fn scroll(&self, delta: i32) {
        let mut t = self.term.lock();
        t.grid_mut().scroll_display(Scroll::Delta(delta));
    }

    fn scroll_to_fraction(&self, fraction: f32) {
        let mut t = self.term.lock();
        let history = t.grid().history_size();
        if history == 0 {
            return;
        }
        let target_offset = ((1.0 - fraction) * history as f32).round() as i32;
        let current_offset = t.grid().display_offset() as i32;
        let delta = target_offset - current_offset;
        t.grid_mut().scroll_display(Scroll::Delta(delta));
    }

    fn start_selection(&self, grid_point: GridPoint, side: Side) {
        let mut t = self.term.lock();
        t.selection = Some(Selection::new(SelectionType::Simple, grid_point, side));
    }

    fn update_selection(&self, grid_point: GridPoint, side: Side) {
        let mut t = self.term.lock();
        if let Some(ref mut sel) = t.selection {
            sel.update(grid_point, side);
        }
    }

    fn clear_selection(&self) {
        let mut t = self.term.lock();
        t.selection = None;
    }

    /// Expand selection to the word around `grid_point` using alacritty's
    /// semantic-search rules. Boundaries come from
    /// `term.config.semantic_escape_chars`, which defaults to the set
    /// `, ¦ ' " ( ) [ ] { } < > tab space`, so this picks up whatever
    /// non-whitespace token you double-clicked on.
    fn select_semantic(&self, grid_point: GridPoint, side: Side) {
        let mut t = self.term.lock();
        t.selection = Some(Selection::new(SelectionType::Semantic, grid_point, side));
    }

    /// Select the entire logical line containing `grid_point`. Used for
    /// triple-click.
    fn select_line(&self, grid_point: GridPoint) {
        let mut t = self.term.lock();
        t.selection = Some(Selection::new(SelectionType::Lines, grid_point, Side::Left));
    }

    /// Current PTY dimensions in cells — exposed so the API snapshot
    /// can ship the per-tab cols/rows to remote viewers (xterm.js
    /// resizes its grid to match, otherwise the server-side wrapping
    /// renders weird when the browser window is wider than the PTY).
    pub fn dims(&self) -> (u16, u16) {
        // Hold the lock only for the two field reads, not for the
        // whole function (clippy:significant_drop_tightening).
        let t = self.term.lock();
        let g = t.grid();
        let cols = g.columns() as u16;
        let rows = g.screen_lines() as u16;
        drop(t);
        (cols, rows)
    }

    /// The grid size this view last laid out at (cols, lines, cell), once it
    /// has been painted at least once. `None` before the first prepaint. The
    /// app reads this off the *active* tab and broadcasts it to the others via
    /// [`Self::force_resize`], so background tabs match the visible one.
    #[must_use]
    pub fn measured_grid(&self) -> Option<(usize, usize, Size<Pixels>)> {
        match (self.last_size.get(), self.cell_size) {
            (Some((cols, lines)), Some(cell)) => Some((cols, lines, cell)),
            _ => None,
        }
    }

    /// Apply a resize parked by the prepaint rate-limit once it has
    /// been stable for [`RESIZE_SETTLE`]. Called from the repaint pump;
    /// returns whether a reflow happened (the caller repaints).
    fn apply_pending_resize(&mut self) -> bool {
        let Some((cols, lines, at)) = self.pending_resize.get() else {
            return false;
        };
        if at.elapsed() < RESIZE_SETTLE {
            return false;
        }
        self.pending_resize.set(None);
        let Some(cell) = self.cell_size else {
            return false;
        };
        self.last_resize_apply.set(Some(std::time::Instant::now()));
        self.force_resize(cols, lines, cell);
        true
    }

    /// Resize the grid + PTY without going through a paint — for background
    /// tabs, which are never mounted and so never hit the prepaint resize.
    /// Mirrors the resize body in [`TerminalElement::prepaint`]; no-op when the
    /// size is unchanged.
    pub fn force_resize(&mut self, cols: usize, lines: usize, cell: Size<Pixels>) {
        let cols = cols.max(2);
        let lines = lines.max(1);
        if self.cell_size.is_none() {
            self.cell_size = Some(cell);
        }
        if self.last_size.get() == Some((cols, lines)) {
            return;
        }
        self.last_size.set(Some((cols, lines)));
        {
            let mut t = self.term.lock();
            t.resize(TermDims {
                columns: cols,
                screen_lines: lines,
            });
        }
        if let Some(n) = self.notifier.as_ref() {
            let _ = n.send(Msg::Resize(WindowSize {
                num_lines: lines as u16,
                num_cols: cols as u16,
                cell_width: f32::from(cell.width) as u16,
                cell_height: f32::from(cell.height) as u16,
            }));
        }
    }

    /// Row-by-row dump for the xterm.js viewer (no WRAPLINE join).
    /// Each server-grid row → one `\n`-terminated line, so the
    /// browser-side terminal at matching cols reproduces the layout
    /// cell-for-cell instead of relying on xterm.js auto-wrap to
    /// re-land the wrap point. Returns (text, optional cursor at
    /// (`row_in_dump`, col)).
    pub fn raw_screen_text(&self, max_lines: Option<usize>) -> (String, Option<(usize, usize)>) {
        crate::term_export::term_to_ansi_rows(&self.term, max_lines)
    }

    pub fn copy_selection(&self) -> Option<String> {
        let t = self.term.lock();
        t.selection_to_string()
    }

    /// Whether an active, non-empty selection exists. For render paths
    /// that only need the yes/no — `copy_selection` builds the whole
    /// selected TEXT under the Term lock, which the context menu used
    /// to do once per frame just to pick a menu label. Non-blocking:
    /// if the parser holds the lock this frame, report "no selection"
    /// and let the next repaint correct it.
    pub fn has_selection(&self) -> bool {
        self.term
            .try_lock_unfair()
            .is_some_and(|t| t.selection.as_ref().is_some_and(|s| !s.is_empty()))
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn copy_all_history(&self) -> String {
        self.ansi_text_with_cursor(None).0
    }

    /// A `Send` closure that serialises this tab's scrollback (capped at
    /// `max_lines`; `None` = full history) to the same string
    /// `copy_all_history` produces. Hands the `Term` `Arc` to a worker
    /// thread so the expensive walk runs OFF the gpui main thread — the
    /// persist tick submits these instead of serialising inline, which is
    /// what used to stall typing every 2 s under many active tabs. The
    /// periodic caller caps the depth (`PERIODIC_OUTPUT_SAVE_LINES`) so
    /// the walk also stops monopolising the Term lock the PARSER needs.
    pub fn history_job(&self, max_lines: Option<usize>) -> impl FnOnce() -> String + Send + 'static {
        let term = self.term.clone();
        move || crate::term_export::term_to_ansi_text_with_cursor(&term, max_lines).0
    }

    /// Same view as `plain_text` but with SGR escape sequences preserved
    /// per fg/bg/flags change, so a remote client can render colours.
    /// `max_lines` caps the result to the last N lines (visible screen
    /// plus scrollback) — pass `None` to dump the full history.
    /// Same as `ansi_text` but also returns the cursor's position in
    /// the *logical* line space (i.e. after wrapped rows have been
    /// joined). Returns None when the cursor is in scrollback or
    /// outside the requested window. Used by the mobile remote to
    /// render the cursor at the right (row, col).
    /// Delegates straight to the shared `term_export` implementation.
    /// This used to round-trip through a `Vec<String>` — split the
    /// joined dump into per-line Strings, then join them again — which
    /// tripled the copies (and added ~10k String allocs for a full
    /// history dump) for byte-identical output.
    pub fn ansi_text_with_cursor(&self, max_lines: Option<usize>) -> (String, Option<(usize, usize)>) {
        crate::term_export::term_to_ansi_text_with_cursor(&self.term, max_lines)
    }

    fn url_at_grid(&self, line: usize, col: usize) -> Option<DetectedUrl> {
        let urls = self.detected_urls.borrow();
        urls.iter()
            .find(|u| u.line == line && col >= u.start_col && col < u.end_col)
            .cloned()
    }

    /// The detected link currently under the mouse cursor (the last hover
    /// grid cell ∩ a detected URL/path), or `None`. Backs the right-click
    /// menu's "Copy path (link)" entry — the hover cell is updated on
    /// mouse-move, so on a right-click it already reflects the clicked cell.
    #[must_use]
    pub fn hovered_url(&self) -> Option<String> {
        let (line, col) = self.hover_grid.get()?;
        self.url_at_grid(line, col).map(|u| u.url.to_string())
    }

    /// Returns viewport-relative grid point and side (Line(0) = top of
    /// visible region, regardless of scrollback offset). Use
    /// `pixel_to_absolute_grid` when the result feeds alacritty's selection,
    /// which stores absolute grid coordinates.
    fn pixel_to_grid(&self, pos: gpui::Point<Pixels>, bounds_origin: gpui::Point<Pixels>) -> (GridPoint, Side) {
        let cell = self.cell_size.unwrap_or(Size {
            width: px(8.4),
            height: px(19.6),
        });
        let x = f32::from(pos.x - bounds_origin.x);
        let y = f32::from(pos.y - bounds_origin.y);
        let col = (x / f32::from(cell.width)).max(0.0) as usize;
        let line = (y / f32::from(cell.height)).max(0.0) as i32;
        let side = if x % f32::from(cell.width) < f32::from(cell.width) / 2.0 {
            Side::Left
        } else {
            Side::Right
        };
        (GridPoint::new(Line(line), Column(col)), side)
    }

    /// Same as `pixel_to_grid` but with the scrollback offset folded in so
    /// the Line value addresses absolute grid content, not screen position.
    /// This is what alacritty's selection model expects — it rotates the
    /// stored absolute lines as new output pushes content around.
    fn pixel_to_absolute_grid(
        &self,
        pos: gpui::Point<Pixels>,
        bounds_origin: gpui::Point<Pixels>,
    ) -> (GridPoint, Side) {
        let (mut gp, side) = self.pixel_to_grid(pos, bounds_origin);
        let off = self.term.lock().grid().display_offset() as i32;
        gp.line = Line(gp.line.0 - off);
        (gp, side)
    }
}

// `sgr_color` was inlined into `crate::term_export::sgr_color` so the
// GUI render + the headless ANSI dump don't drift. Tests below
// import the shared one directly.

/// Measure a monospace cell (advance width × line height) for `fc` through the
/// window's text system — the same shaping pipeline `shape_line` uses, so the
/// grid maths matches what's painted. Extracted so callers that don't render a
/// terminal (e.g. the app computing a startup grid size for every tab) can size
/// the PTY without mounting the view.
///
/// Cell width measurement based on Zed's `terminal_element.rs`
/// Copyright (c) Zed Industries — Apache-2.0 / GPL-3.0
#[must_use]
pub fn measure_cell(window: &mut Window, fc: &FontConfig) -> Size<Pixels> {
    let mut f = font(fc.family.clone());
    f.weight = FontWeight(fc.weight as f32);
    let font_size = px(fc.size);
    let text_sys = window.text_system();
    // Measure cell width through the shaping pipeline to match shape_line.
    let layout = text_sys.layout_line(
        "m",
        font_size,
        &[TextRun {
            len: 1,
            font: f,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    Size {
        width: layout.width,
        height: font_size * 1.4,
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Mark this view as the foreground one this frame. Only the active
        // tab is mounted, so this stamp only advances for the visible tab;
        // the repaint pump uses its staleness to mute background tabs.
        //
        // First frame after a gap → force a full rebuild. The prepaint reuses
        // last frame's rows for cells alacritty didn't mark damaged. That's
        // only safe while we paint continuously: a tab that went un-painted
        // (parked, window hidden, or just switched to) may have had an in-place
        // TUI redraw (Claude Code re-opening, htop, …) whose damage was never
        // consumed, so the cached rows are stale and reuse would leave the
        // screen half-painted until a keystroke re-damaged it. Dropping the
        // frame cache here makes this paint rebuild every row from the live
        // grid. Steady-state painting (gap < REPAINT_FULL_GAP) is untouched, so
        // the damage optimisation still applies to the common case.
        const REPAINT_FULL_GAP: Duration = Duration::from_secs(2);
        if self.last_render.get().is_none_or(|t| t.elapsed() >= REPAINT_FULL_GAP) {
            self.release_render_caches();
        }
        self.last_render.set(Some(std::time::Instant::now()));
        let focus = self.focus.clone();
        let term = self.term.clone();

        // Measure cell size once we have a text system.
        if self.cell_size.is_none() {
            self.cell_size = Some(measure_cell(window, &self.font_config));
        }

        let cell_size = self.cell_size.unwrap_or(Size {
            width: px(8.4),
            height: px(19.6),
        });

        div()
            .id("terminal")
            .key_context("terminal")
            .track_focus(&focus)
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                // T0 — instrumentation: keystroke entered the gpui
                // listener. Enable with RUST_LOG=tab_atelier::input_lag=trace.
                // Compile to a no-op in release when trace isn't enabled.
                trace!(
                    target: crate::INPUT_TRACE_TARGET,
                    "T0 keystroke key={:?} key_char={:?} held={} ctrl={} shift={} alt={} fn={}",
                    ks.key,
                    ks.key_char,
                    ev.is_held,
                    ks.modifiers.control,
                    ks.modifiers.shift,
                    ks.modifiers.alt,
                    ks.modifiers.function,
                );
                if ks.modifiers.control && ks.modifiers.shift {
                    match ks.key.as_str() {
                        "c" => {
                            if let Some(text) = this.copy_selection() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                            return;
                        }
                        "v" => {
                            if let Some(item) = cx.read_from_clipboard()
                                && let Some(text) = Self::clipboard_to_paste_text(&item)
                            {
                                this.send_clipboard(&text);
                            }
                            return;
                        }
                        "t" => return,
                        _ => {}
                    }
                }
                if ks.modifiers.alt && ks.key.as_str() == "tab" {
                    return;
                }
                if ks.modifiers.shift && !ks.modifiers.control {
                    match ks.key.as_str() {
                        "pageup" => {
                            // Shift+PgUp ⇒ scroll back into history.
                            // alacritty's `Scroll::Delta(+N)` increases
                            // display_offset (= older content visible);
                            // see the on_scroll_wheel port above.
                            this.scroll(this.visible_lines() as i32);
                            return;
                        }
                        "pagedown" => {
                            // Shift+PgDn ⇒ scroll toward newest content.
                            this.scroll(-(this.visible_lines() as i32));
                            return;
                        }
                        "home" => {
                            this.scroll_to_fraction(0.0);
                            return;
                        }
                        "end" => {
                            this.scroll_to_fraction(1.0);
                            return;
                        }
                        "insert" => {
                            if let Some(item) = cx.read_from_clipboard()
                                && let Some(text) = Self::clipboard_to_paste_text(&item)
                            {
                                this.send_clipboard(&text);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                if ks.modifiers.control && !ks.modifiers.shift && ks.key.as_str() == "insert" {
                    if let Some(text) = this.copy_selection() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return;
                }
                if ks.modifiers.control && ks.modifiers.shift && ks.key.as_str() == "l" {
                    this.send_input(b"\x1b[2J\x1b[H".to_vec());
                    return;
                }
                let mode = this.term.lock().mode().to_owned();
                if let Some(bytes) = keystroke_to_bytes(&ev.keystroke, mode) {
                    this.clear_selection();
                    this.send_input(bytes);
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _window, cx| {
                // Ported from Zed's `terminal::scroll_wheel` +
                // `determine_scroll_lines`. Three things our previous
                // implementation got wrong:
                //
                // 1. We negated `lines` before calling `Scroll::Delta`,
                //    so the sign convention was inverted relative to
                //    alacritty's (positive = scroll back into history).
                //    Dropped — pass the raw delta through, same as Zed.
                //
                // 2. We never reset `scroll_acc` on TouchPhase::Started,
                //    so the float carried over between gestures and the
                //    first scroll after a direction change felt sticky.
                //    Now: reset on Started, ignore Ended, work on Moved.
                //
                // 3. We always scrolled the LOCAL viewport. When a TUI
                //    (less, vim, htop) opted into alt-screen with
                //    ALTERNATE_SCROLL set, the user expected wheel
                //    events to scroll the TUI's own buffer — Zed
                //    translates the wheel into `\x1bOA` / `\x1bOB`
                //    (up / down arrows) and ships them to the PTY so
                //    the TUI reacts. Now we do the same.
                match ev.touch_phase {
                    TouchPhase::Started => {
                        this.scroll_acc.set(0.0);
                    }
                    TouchPhase::Moved => {
                        let line_h = this.cell_size.map_or(px(19.6), |c| c.height);
                        let multiplier = this.font_config.scroll_sensitivity;
                        let delta_px = ev.delta.pixel_delta(line_h);
                        let old_offset = (this.scroll_acc.get() / f32::from(line_h)) as i32;
                        let acc = this.scroll_acc.get() + f32::from(delta_px.y) * multiplier;
                        let new_offset = (acc / f32::from(line_h)) as i32;
                        let total_h = f32::from(line_h) * 100.0;
                        this.scroll_acc.set(acc % total_h);
                        let lines = new_offset - old_offset;
                        if lines == 0 {
                            return;
                        }
                        // `Term::mode()` returns `&TermMode` (a borrow
                        // into the FairMutex guard). Copy out before
                        // the guard drops — TermMode is bitflags-derived
                        // and so is Copy.
                        let mode = *this.term.lock().mode();
                        let alt_scroll =
                            mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !ev.modifiers.shift;
                        if alt_scroll {
                            this.send_input(alt_scroll_bytes(lines));
                        } else {
                            this.scroll(lines);
                        }
                        cx.notify();
                    }
                    TouchPhase::Ended => {}
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, _cx| {
                    // Clicking the terminal MUST take focus. `track_focus`
                    // above wires the FocusHandle, but it is only ever
                    // focused programmatically (initial mount, tab
                    // switch, dismissing prefs/rename/hotkey-picker), so
                    // any prior focus shift to another input leaves the
                    // terminal eating no keys — that's the "Enter doesn't
                    // work / typing is broken" report. Focusing on click
                    // matches every other terminal/text-input app.
                    this.focus.focus(window);
                    let origin = this.content_origin.get();
                    let bounds = this.bounds_size.get();
                    let scrollbar_left = origin.x + bounds.width - px(SCROLLBAR_WIDTH);
                    if ev.position.x >= scrollbar_left {
                        this.scrollbar_dragging.set(true);
                        this.click_origin.set(None);
                        let y_frac = f32::from(ev.position.y - origin.y) / f32::from(bounds.height);
                        this.scroll_to_fraction(y_frac.clamp(0.0, 1.0));
                    } else {
                        // Record click_origin in viewport coordinates (so
                        // double-click-on-link detection compares like with
                        // like) but feed selection the absolute coordinates.
                        let (vp_gp, vp_side) = this.pixel_to_grid(ev.position, origin);
                        this.click_origin.set(Some(vp_gp));
                        let (abs_gp, _) = this.pixel_to_absolute_grid(ev.position, origin);
                        this.start_selection(abs_gp, vp_side);
                    }
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _window, cx| {
                let origin = this.content_origin.get();
                // If the button was released while the cursor was outside our
                // element bounds, on_mouse_up doesn't fire — so the drag flag
                // would otherwise stick. Reconcile against the actual button
                // state on every move.
                if this.scrollbar_dragging.get() && ev.pressed_button != Some(MouseButton::Left) {
                    this.scrollbar_dragging.set(false);
                }
                if this.scrollbar_dragging.get() {
                    let bounds = this.bounds_size.get();
                    let y_frac = f32::from(ev.position.y - origin.y) / f32::from(bounds.height);
                    this.scroll_to_fraction(y_frac.clamp(0.0, 1.0));
                } else if ev.pressed_button == Some(MouseButton::Left) {
                    let (gp, side) = this.pixel_to_absolute_grid(ev.position, origin);
                    this.update_selection(gp, side);
                } else {
                    let (gp, _) = this.pixel_to_grid(ev.position, origin);
                    let line = gp.line.0.max(0) as usize;
                    let col = gp.column.0;
                    let prev = this.hover_grid.get();
                    let new = Some((line, col));
                    if prev != new {
                        this.hover_grid.set(new);
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseUpEvent, _window, _cx| {
                    this.scrollbar_dragging.set(false);
                    if let Some(origin_gp) = this.click_origin.take() {
                        let origin = this.content_origin.get();
                        let (gp, _) = this.pixel_to_grid(ev.position, origin);
                        // Triple-click selects the whole logical line; falls
                        // through neither to URL-open nor word-select. The
                        // 33 ms repaint loop picks the new selection up; no
                        // explicit cx.notify() needed (the listener is Fn,
                        // not FnMut).
                        if origin_gp == gp && ev.click_count >= 3 {
                            let (abs_gp, _) = this.pixel_to_absolute_grid(ev.position, origin);
                            this.select_line(abs_gp);
                            return;
                        }
                        if origin_gp == gp && ev.click_count == 2 {
                            let line = gp.line.0.max(0) as usize;
                            let col = gp.column.0;
                            // Double-click on a URL opens it; anywhere else
                            // expands the selection to the surrounding word
                            // using alacritty's semantic-escape boundaries
                            // (space / quotes / brackets / parens / pipes…).
                            if this.url_at_grid(line, col).is_none() {
                                let (abs_gp, side) = this.pixel_to_absolute_grid(ev.position, origin);
                                this.select_semantic(abs_gp, side);
                                return;
                            }
                        }
                        if origin_gp == gp && ev.click_count >= 2 {
                            let line = gp.line.0.max(0) as usize;
                            let col = gp.column.0;
                            if let Some(url) = this.url_at_grid(line, col) {
                                let browser = this.browser.borrow().clone();
                                if url.is_file {
                                    let raw = file_path_for_open(&url.url);
                                    let path = std::path::Path::new(raw);
                                    let resolved = if let Some(tail) = raw.strip_prefix("~/")
                                        && let Some(home) = std::env::var_os("HOME")
                                    {
                                        std::path::PathBuf::from(home).join(tail)
                                    } else if raw == "~"
                                        && let Some(home) = std::env::var_os("HOME")
                                    {
                                        std::path::PathBuf::from(home)
                                    } else if let Some(rest) = raw.strip_prefix('$')
                                        && let Some(slash) = rest.find('/')
                                        && let Some(val) = std::env::var_os(&rest[..slash])
                                    {
                                        std::path::PathBuf::from(val).join(&rest[slash + 1..])
                                    } else if path.is_absolute() {
                                        path.to_path_buf()
                                    } else if let Some(cwd) = crate::platform::process_cwd(this.pid) {
                                        cwd.join(path)
                                    } else {
                                        path.to_path_buf()
                                    };
                                    let ext = resolved
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .unwrap_or("")
                                        .to_ascii_lowercase();
                                    let open_in_viewer = matches!(
                                        ext.as_str(),
                                        "html"
                                            | "htm"
                                            | "pdf"
                                            | "png"
                                            | "jpg"
                                            | "jpeg"
                                            | "gif"
                                            | "svg"
                                            | "webp"
                                            | "mp4"
                                            | "mp3"
                                            | "webm"
                                            | "avi"
                                            | "mkv"
                                    );
                                    let open_with_system = matches!(
                                        ext.as_str(),
                                        "deb"
                                            | "rpm"
                                            | "appimage"
                                            | "flatpakref"
                                            | "iso"
                                            | "dmg"
                                            | "pkg"
                                            | "zip"
                                            | "tar"
                                            | "gz"
                                            | "bz2"
                                            | "xz"
                                            | "7z"
                                            | "rar"
                                    );
                                    if open_in_viewer {
                                        info!("opening in viewer: {}", resolved.display());
                                        crate::platform::open_url(&resolved.to_string_lossy(), browser.as_deref());
                                    } else if open_with_system {
                                        info!("opening with system handler: {}", resolved.display());
                                        crate::platform::open_path(&resolved, None);
                                    } else {
                                        let editor = this.code_editor.borrow().clone();
                                        info!("opening file: {}", resolved.display());
                                        crate::platform::open_path(&resolved, editor.as_deref());
                                    }
                                } else {
                                    info!("opening URL: {}", url.url);
                                    crate::platform::open_url(&url.url, browser.as_deref());
                                }
                                this.clear_selection();
                            }
                        }
                    }
                }),
            )
            .size_full()
            .child(TerminalElement {
                term,
                notifier: self.notifier.clone(),
                cell_size,
                last_size: self.last_size.clone(),
                pending_resize: self.pending_resize.clone(),
                last_resize_apply: self.last_resize_apply.clone(),
                content_origin: self.content_origin.clone(),
                bounds_size: self.bounds_size.clone(),
                line_cache: self.line_cache.clone(),
                line_cache_scratch: self.line_cache_scratch.clone(),
                theme: self.theme,
                font_config: self.font_config.clone(),
                detected_urls: self.detected_urls.clone(),
                hover_grid: self.hover_grid.clone(),
                prev_frame: self.prev_frame.clone(),
                last_prepaint: self.last_prepaint.clone(),
                pty_ring: self.pty_ring.clone(),
            })
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

struct TerminalElement {
    term: Arc<FairMutex<Term<EventProxy>>>,
    /// `None` only for a not-yet-spawned skeleton; the mounted (active) tab is
    /// always spawned, so in practice the element that paints has `Some`.
    notifier: Option<EventLoopSender>,
    cell_size: Size<Pixels>,
    last_size: Rc<Cell<Option<(usize, usize)>>>,
    /// Resize parked during a storm: `(cols, lines, requested_at)` —
    /// applied by [`Self::apply_pending_resize`] once stable. See
    /// [`RESIZE_SETTLE`].
    pending_resize: Rc<Cell<Option<(usize, usize, std::time::Instant)>>>,
    /// When the grid was last actually reflowed (leading-edge stamp).
    last_resize_apply: Rc<Cell<Option<std::time::Instant>>>,
    content_origin: Rc<Cell<gpui::Point<Pixels>>>,
    bounds_size: Rc<Cell<Size<Pixels>>>,
    line_cache: Rc<RefCell<HashMap<i32, CachedLine>>>,
    line_cache_scratch: Rc<RefCell<HashMap<i32, CachedLine>>>,
    theme: ThemeName,
    font_config: FontConfig,
    detected_urls: Rc<RefCell<Vec<DetectedUrl>>>,
    hover_grid: Rc<Cell<Option<(usize, usize)>>>,
    prev_frame: Rc<RefCell<Option<CachedFrame>>>,
    last_prepaint: Rc<RefCell<Option<TermPrepaint>>>,
    /// Same ring as [`TerminalView::pty_ring`]; prepaint reads its byte
    /// counter as the "did any output arrive since the cached frame was
    /// built" signal for [`CachedFrame::shift_for`].
    pty_ring: Arc<std::sync::Mutex<crate::pty_ring::PtyRing>>,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

#[derive(Clone)]
struct TermPrepaint {
    lines: Vec<TermLine>,
    cursor: Option<(usize, usize)>,
    selection: Option<SelectionRange>,
    visible_cols: usize,
    display_offset: usize,
    history_size: usize,
}

use alacritty_terminal::selection::SelectionRange;

#[derive(Clone)]
struct TermSegment {
    col_start: usize,
    shaped: ShapedLine,
}

#[derive(Clone)]
struct TermLine {
    /// Shaped glyphs for this row. `Rc` so cloning a `TermPrepaint`
    /// (the try-lock reuse path + the prev-frame cache) is a cheap
    /// refcount bump, not a deep `ShapedLine` copy.
    segments: std::rc::Rc<Vec<TermSegment>>,
    /// The Phase 1 row this line was built from. `paint` reads
    /// `bg_runs` / `box_cells` through it — they used to be deep-cloned
    /// out of the shared row twice per line per frame (once into
    /// `TermLine`, once more when `TermPrepaint` is cloned for the
    /// try-lock reuse stash).
    raw: Rc<RawLine>,
}

#[derive(Clone)]
struct BgRun {
    col: usize,
    len: usize,
    color: Hsla,
}

/// A box-drawing cell to paint as connecting bars rather than via the
/// (usually too-narrow, gappy) font glyph. See [`crate::box_drawing`].
#[derive(Clone)]
struct BoxCell {
    col: usize,
    parts: crate::box_drawing::BoxParts,
    color: Hsla,
}

/// One merged background rectangle in grid coordinates — the output of
/// [`merge_bg_rects`], painted as a single quad.
#[derive(Clone, Copy, PartialEq, Debug)]
struct BgRect {
    col: usize,
    row: usize,
    len: usize,
    rows: usize,
    color: Hsla,
}

/// Merge per-row background runs into vertical spans: a run whose
/// (col, len, color) matches one directly above extends that rectangle
/// downward instead of emitting its own quad. TUI blocks paint the
/// same run on dozens of consecutive rows (Claude Code's message
/// bubbles / diff views, panel fills) — one GPU quad instead of one
/// per row. Runs within a row are disjoint, so merged rects are too.
fn merge_bg_rects<'a>(rows: impl Iterator<Item = &'a [BgRun]>) -> Vec<BgRect> {
    let mut done: Vec<BgRect> = Vec::new();
    // Rects whose bottom edge touches the previous row (extendable).
    let mut open: Vec<BgRect> = Vec::new();
    let mut next_open: Vec<BgRect> = Vec::new();
    for (row, runs) in rows.enumerate() {
        for run in runs {
            let extended = open.iter().position(|r| {
                r.col == run.col && r.len == run.len && r.row + r.rows == row && hsla_eq(r.color, run.color)
            });
            if let Some(i) = extended {
                let mut r = open.swap_remove(i);
                r.rows += 1;
                next_open.push(r);
            } else {
                next_open.push(BgRect {
                    col: run.col,
                    row,
                    len: run.len,
                    rows: 1,
                    color: run.color,
                });
            }
        }
        // Whatever wasn't extended this row can never grow again.
        done.append(&mut open);
        std::mem::swap(&mut open, &mut next_open);
    }
    done.append(&mut open);
    done
}

/// One same-style run of glyphs inside a [`RawLine`]. Output of
/// Phase 1 (cell scan) and input to Phase 2 (`shape_line`).
#[derive(Clone)]
struct RawSegment {
    col_start: usize,
    text: String,
    runs: Vec<TextRun>,
    /// Cells this segment spans. 1 for normal runs; 2 for a wide-char
    /// segment (CJK / 2-cell emoji like ✅). Passed to `shape_line` as
    /// `force_width = cell.width * cell_span` so the clamp matches the
    /// actual grid span.
    cell_span: usize,
}

/// One screen row's worth of Phase 1 output — cached in
/// [`TerminalView::prev_frame`] for damage-driven re-use.
#[derive(Clone)]
struct RawLine {
    /// Memoised [`line_style_sig`] of `segments` — computed once on the
    /// row's first Phase 2 encounter (outside the Term lock) and reused
    /// for the cached row's whole lifetime. Recomputing it walked every
    /// visible row's runs on EVERY paint, cache hit or not.
    style_sig_memo: Cell<Option<u64>>,
    /// Content-stable line coordinate: `history_size - display_offset +
    /// viewport_row`, i.e. the row's distance from the top of the
    /// scrollback. Unlike a viewport index it does NOT change when the
    /// user scrolls or when streaming pushes lines into history, so the
    /// Phase-2 shaping cache keyed on it survives both — previously
    /// every scroll step and every streaming frame re-shaped ALL
    /// visible rows because the key was viewport-anchored. When the
    /// scrollback ring is full the coordinate saturates (content
    /// rotates under a pinned `history_size`); the cache's text +
    /// style-sig validation turns that into misses, never stale glyphs.
    abs_line: i32,
    text: String,
    segments: Vec<RawSegment>,
    bg_runs: Vec<BgRun>,
    box_cells: Vec<BoxCell>,
}

#[inline]
const fn fnv_mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x0000_0100_0000_01b3)
}

impl RawLine {
    /// Memoised style signature — see the `style_sig_memo` field.
    fn style_sig(&self) -> u64 {
        if let Some(s) = self.style_sig_memo.get() {
            return s;
        }
        let s = line_style_sig(&self.segments);
        self.style_sig_memo.set(Some(s));
        s
    }
}

/// FNV-1a fingerprint of a line's *shaping inputs* — the per-run foreground
/// colour, font weight/style, and under/strikethrough presence. The Phase-2
/// [`CachedLine`] cache is keyed on `abs_line` and used to be validated on
/// `text` alone, so a row whose text was unchanged but whose colours changed
/// reused the previously-shaped glyphs in the stale colour.
///
/// That is the "pasted text is invisible until you edit it" bug: readline
/// wraps a bracketed paste in reverse-video (`\x1b[7m…\x1b[27m`), which this
/// renderer shapes in the *background* colour (with a matching bg run behind
/// it). The instant the active region deactivates — any cursor move or
/// keypress — readline re-emits the SAME characters without the reverse, so
/// the fresh row has no bg run but the text-only cache hit kept the glyphs
/// painted in the background colour → invisible. Editing changed the text and
/// missed the cache, which is why a single keystroke "fixed" it. Folding the
/// colour into the cache key makes the deactivation redraw a cache miss.
fn line_style_sig(segments: &[RawSegment]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for seg in segments {
        h = fnv_mix(h, seg.col_start as u64);
        h = fnv_mix(h, seg.cell_span as u64);
        for run in &seg.runs {
            h = fnv_mix(h, run.len as u64);
            let c = run.color;
            h = fnv_mix(h, u64::from(c.h.to_bits()));
            h = fnv_mix(h, u64::from(c.s.to_bits()));
            h = fnv_mix(h, u64::from(c.l.to_bits()));
            h = fnv_mix(h, u64::from(c.a.to_bits()));
            h = fnv_mix(h, u64::from(run.font.weight.0.to_bits()));
            h = fnv_mix(h, matches!(run.font.style, FontStyle::Italic) as u64);
            h = fnv_mix(h, run.underline.is_some() as u64);
            h = fnv_mix(h, run.strikethrough.is_some() as u64);
        }
    }
    h
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<TermPrepaint>;

    fn id(&self) -> Option<ElementId> {
        Some("terminal-grid".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = window.request_layout(
            Style {
                size: Size {
                    width: relative(1.0).into(),
                    height: relative(1.0).into(),
                },
                ..Default::default()
            },
            [],
            cx,
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let paint_t0 = std::time::Instant::now();
        let cell = self.cell_size;
        let cols = ((bounds.size.width / cell.width) as usize).max(2);
        let lines = ((bounds.size.height / cell.height) as usize).max(1);

        if self.last_size.get() == Some((cols, lines)) {
            // Bounds settled back onto the applied grid mid-storm —
            // nothing left to apply.
            self.pending_resize.set(None);
        } else {
            // See RESIZE_SETTLE: leading edge reflows immediately, a
            // storm parks the newest size for the pump to apply. Until
            // then the old grid keeps painting — the scan below clamps
            // to min(grid, bounds) and the content mask clips, so a
            // transiently mismatched frame is safe.
            let quiet = self
                .last_resize_apply
                .get()
                .is_none_or(|t| t.elapsed() >= RESIZE_SETTLE);
            if quiet {
                self.pending_resize.set(None);
                self.last_resize_apply.set(Some(std::time::Instant::now()));
                self.last_size.set(Some((cols, lines)));
                {
                    let mut t = self.term.lock();
                    t.resize(TermDims {
                        columns: cols,
                        screen_lines: lines,
                    });
                }
                if let Some(n) = self.notifier.as_ref() {
                    let _ = n.send(Msg::Resize(WindowSize {
                        num_lines: lines as u16,
                        num_cols: cols as u16,
                        cell_width: f32::from(cell.width) as u16,
                        cell_height: f32::from(cell.height) as u16,
                    }));
                }
            } else {
                self.pending_resize.set(Some((cols, lines, std::time::Instant::now())));
            }
        }

        self.content_origin.set(bounds.origin);
        self.bounds_size.set(bounds.size);

        let mut mono_font = font(self.font_config.family.clone());
        mono_font.weight = FontWeight(self.font_config.weight as f32);
        let font_size = px(self.font_config.size);
        // Only four Font values can ever come out of the cell scan:
        // (normal | bold) × (normal | italic). Build them ONCE per frame
        // and clone per cell — `gpui::font()` default-constructs
        // `FontFeatures(Arc<Vec<…>>)`, i.e. a heap allocation, and the
        // old code did that for EVERY non-spacer cell of every damaged
        // row while holding the Term lock (a full-damage 250×60 frame is
        // ~15k mallocs; a 30 fps TUI redraw ~450k/s). A Font clone is
        // refcount bumps only.
        let font_variants: [gpui::Font; 4] = {
            let mut v: [gpui::Font; 4] = [mono_font.clone(), mono_font.clone(), mono_font.clone(), mono_font];
            v[1].weight = FontWeight::BOLD; // bold
            v[2].style = FontStyle::Italic; // italic
            v[3].weight = FontWeight::BOLD; // bold italic
            v[3].style = FontStyle::Italic;
            v
        };
        let t = theme::theme(self.theme);
        let fg_default = t.term_fg_hsla();

        // Phase 1: read cell data under the lock — no shaping here.
        //
        // Damage-driven row reuse (Ghostty `rebuildCells` pattern):
        // pull `Term::damage()`, keep last frame's per-row `RawLine`s
        // for rows that weren't touched, only walk the cells of rows
        // alacritty marked dirty since the last paint. Typing one
        // character usually damages 1-2 rows (the cursor row, plus
        // the previous-prompt row when the shell scrolls), so the
        // per-frame cell-scan cost drops from O(rows × cols) to
        // O(damaged_rows × cols).
        //
        // Cache compatibility / row realignment: `CachedFrame::shift_for`.
        // A resize rebuilds everything; a pure user scroll (no PTY bytes
        // since the cache was built) SHIFTS the cached rows to their new
        // slots instead of rescanning them; a stationary window reuses
        // rows index-for-index under damage.

        // Read the ring byte counter BEFORE taking the Term lock (the
        // pump and the PTY tap take the ring lock on their own, so
        // never nest it inside the Term lock). Bytes are counted into
        // the ring before they are parsed, so a reuse check that sees
        // an unchanged counter can only be wrong in the safe direction
        // (spurious rebuild), never by missing arrived output.
        let ring_len = self.pty_ring.lock().map_or(0, |r| r.total_len());

        // Non-blocking lock acquisition. During a heavy flood the
        // alacritty PTY-reader thread holds the Term lock for the
        // duration of a large parse batch; a blocking `lock()` here
        // made the UI thread wait out that whole batch (measured
        // p99 ≈ 540 ms prepaint spikes during a `seq 1 2000000`
        // scroll). Instead, if the parser holds it we reuse the last
        // built frame and let the next 16 ms tick try again — the UI
        // thread never blocks on the parser. `_unfair` skips the
        // fairness lease, which is correct for a transient read that
        // gives up immediately on contention.
        let phase1_t0 = std::time::Instant::now();
        let Some(mut term) = self.term.try_lock_unfair() else {
            paint_log::record(paint_log::Sample {
                total: paint_t0.elapsed(),
                phase1: phase1_t0.elapsed(),
                phase2: Duration::ZERO,
            });
            return self.last_prepaint.borrow().clone();
        };
        let (raw_lines, cursor, selection, visible_cols, display_offset_val, history_size) = {
            let display_offset = term.grid().display_offset() as i32;
            let visible_lines = term.grid().screen_lines().min(lines);
            let visible_cols = term.grid().columns().min(cols);
            // Scrollback length now, one input to `shift_for` below: a
            // live-screen scroll grows this while display_offset stays 0
            // (rows shift up), a user scroll changes display_offset while
            // this stays put — their difference is the content-stable
            // base the cached rows are realigned against.
            let history_size = term.grid().history_size();
            let cursor_point = term.grid().cursor.point;
            // Honour the app's cursor-visibility mode. Every TUI hides
            // the cursor with `\x1b[?25l` while it redraws (piu-piu,
            // vim, htop, less, …); without this check we painted the
            // cursor block anyway, wherever the app last left it —
            // visible as a cursor "bouncing around" the screen during
            // an animated redraw. SHOW_CURSOR is set by default and
            // cleared by `?25l`, restored by `?25h`.
            let cursor_visible = term.mode().contains(TermMode::SHOW_CURSOR);

            // Collect damage BEFORE we re-borrow the grid immutably
            // for the cell scan. `Term::damage()` returns either Full
            // (entire screen) or Partial (iter of damaged-row indices
            // in viewport coords). After consuming, `reset_damage`
            // clears so next paint sees only NEW changes.
            let (force_full, damaged_rows) = {
                use alacritty_terminal::term::TermDamage;
                let mut rows = vec![false; visible_lines];
                let mut force = false;
                match term.damage() {
                    TermDamage::Full => force = true,
                    TermDamage::Partial(iter) => {
                        for d in iter {
                            // `d.line` IS the viewport row: alacritty
                            // stores damage per screen line, and the
                            // TermDamageIterator reports it shifted by
                            // display_offset (screen line r is shown at
                            // viewport row r + offset), truncating rows
                            // pushed below the viewport. So this maps
                            // correctly even while scrolled into
                            // history — live-screen edits invalidate
                            // exactly the rows they're visible on.
                            if d.line < rows.len() {
                                rows[d.line] = true;
                            }
                        }
                    }
                }
                term.reset_damage();
                (force, rows)
            };

            // Take prev frame's `Vec<Option<RawLine>>` verbatim (no
            // intermediate alloc). Mark damaged rows as None so the
            // build loop below rebuilds them; un-damaged slots stay
            // populated and skip the rebuild entirely.
            let prev = self.prev_frame.borrow_mut().take();
            let shift = prev
                .as_ref()
                .and_then(|p| p.shift_for(display_offset, visible_cols, visible_lines, history_size, ring_len));
            let mut working: Vec<Option<Rc<RawLine>>> = match (prev, shift) {
                (Some(p), Some(0)) => {
                    let mut v = p.lines;
                    if force_full {
                        v.fill_with(|| None);
                    } else {
                        for (i, dmg) in damaged_rows.iter().enumerate() {
                            if *dmg && i < v.len() {
                                v[i] = None;
                            }
                        }
                    }
                    v.resize_with(visible_lines, || None);
                    v
                }
                (Some(p), Some(shift)) => {
                    // Pure scroll: identical content through a moved
                    // window. Realign the rows; only the newly exposed
                    // slots (shifted in from outside the old window)
                    // are rescanned. `force_full` is IGNORED here — it
                    // is the full damage alacritty marks for every
                    // `scroll_display`, and the ring counter already
                    // proved no bytes arrived to change the content —
                    // but partial damage (a parser flush that raced the
                    // cache build) still invalidates its rows.
                    let mut v = p.lines;
                    v.resize_with(visible_lines, || None);
                    shift_rows(&mut v, shift);
                    for (i, dmg) in damaged_rows.iter().enumerate() {
                        if *dmg && i < v.len() {
                            v[i] = None;
                        }
                    }
                    v
                }
                _ => vec![None; visible_lines],
            };

            let grid = term.grid();
            for (l, slot) in working.iter_mut().enumerate().take(visible_lines) {
                if slot.is_some() {
                    continue;
                }
                let grid_line = l as i32 - display_offset;
                let abs_line = history_size as i32 + grid_line;
                let mut full_text = String::with_capacity(visible_cols);
                let mut segments: Vec<RawSegment> = Vec::new();
                let mut cur_seg: Option<RawSegment> = None;
                let mut bg_runs: Vec<BgRun> = Vec::new();
                let mut box_cells: Vec<BoxCell> = Vec::new();

                for c in 0..visible_cols {
                    let cell_data = &grid[GridPoint::new(Line(grid_line), Column(c))];
                    if cell_data.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        full_text.push(' ');
                        // Extend (or open) the trailing bg run so it
                        // covers BOTH halves of the wide glyph. The
                        // spacer carries alacritty's "this is the
                        // right half of a 2-cell char" semantics —
                        // its bg field equals the wide char's, so a
                        // 1-cell run on the wide char alone (the old
                        // code's behaviour) left the right half of
                        // every coloured emoji / CJK glyph painted in
                        // the tab's default bg. Visible on the
                        // Claude Code "auto mode" footer:
                        //   ⏵⏵ ← yellow bg only on the LEFT half of
                        //        each triangle, blue (tab bg) on the
                        //        right half.
                        if cell_data.flags.contains(CellFlags::INVERSE) {
                            let prev_fg = if is_default_fg(cell_data.fg) {
                                fg_default
                            } else {
                                t.color_to_hsla(cell_data.fg)
                            };
                            if let Some(last) = bg_runs.last_mut()
                                && last.col + last.len == c
                                && hsla_eq(last.color, prev_fg)
                            {
                                last.len += 1;
                            }
                        } else if !is_default_bg(cell_data.bg) {
                            let bg_c = t.color_to_hsla(cell_data.bg);
                            if let Some(last) = bg_runs.last_mut()
                                && last.col + last.len == c
                                && hsla_eq(last.color, bg_c)
                            {
                                last.len += 1;
                            }
                        }
                        continue;
                    }
                    let ch = if cell_data.c == '\0' { ' ' } else { cell_data.c };
                    full_text.push(ch);
                    // Wide chars (CJK ideographs, hiragana, katakana, hangul,
                    // most emoji) occupy 2 grid columns: the char itself in
                    // cell N + a WIDE_CHAR_SPACER in cell N+1. The text-shape
                    // engine renders them at ~2× cell_width advance. If a
                    // wide char is merged into a regular ASCII segment, the
                    // segment's apparent advance grows past
                    // col_start * cell_width + len * cell_width and every
                    // char that follows it inside the same shape drifts one
                    // cell right (visually overlapping with the next
                    // segment). Emit wide chars as standalone segments —
                    // shaped once, painted at exactly col_start * cell_width,
                    // and the next regular segment starts cleanly at the
                    // grid-aligned column after the SPACER. Narrow non-ASCII
                    // (é, è, ┌, ├) is unaffected — those stay merged so
                    // gpui's shape_line handles font fallback in one pass.
                    let is_wide = cell_data.flags.contains(CellFlags::WIDE_CHAR);

                    let mut fg = if is_default_fg(cell_data.fg) {
                        fg_default
                    } else {
                        t.color_to_hsla(cell_data.fg)
                    };

                    // Box-drawing glyphs from the font are usually narrower
                    // than the cell, so long runs render dashed and column
                    // rules drift. Divert the common subset to geometric
                    // rendering (connecting bars painted in Phase 2). This is
                    // computed before the INVERSE handling below: an inverse
                    // box cell must NOT take the fg/bg swap — inverting a thin
                    // rule turns it into a filled block (and falls back to the
                    // gappy glyph), which is what made reverse-video scrollbar
                    // rules render dashed. Draw the rule geometrically in the
                    // normal foreground instead, with no inverse bg fill.
                    let box_parts = crate::box_drawing::parts(ch);

                    // Index into `font_variants`: bit 0 = bold, bit 1 = italic.
                    let mut font_idx = 0usize;
                    let mut underline = None;
                    let mut strikethrough = None;

                    if cell_data.flags.contains(CellFlags::BOLD) {
                        font_idx |= 1;
                    }
                    if cell_data.flags.contains(CellFlags::ITALIC) {
                        font_idx |= 2;
                    }
                    if cell_data.flags.intersects(CellFlags::ALL_UNDERLINES) {
                        underline = Some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(fg),
                            wavy: cell_data.flags.contains(CellFlags::UNDERCURL),
                        });
                    }
                    if cell_data.flags.contains(CellFlags::STRIKEOUT) {
                        strikethrough = Some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: Some(fg),
                        });
                    }
                    if cell_data.flags.contains(CellFlags::INVERSE) && box_parts.is_none() {
                        let bg_c = if is_default_bg(cell_data.bg) {
                            t.term_bg_hsla()
                        } else {
                            t.color_to_hsla(cell_data.bg)
                        };
                        let old_fg = fg;
                        fg = bg_c;
                        if let Some(last) = bg_runs.last_mut() {
                            if last.col + last.len == c && hsla_eq(last.color, old_fg) {
                                last.len += 1;
                            } else {
                                bg_runs.push(BgRun {
                                    col: c,
                                    len: 1,
                                    color: old_fg,
                                });
                            }
                        } else {
                            bg_runs.push(BgRun {
                                col: c,
                                len: 1,
                                color: old_fg,
                            });
                        }
                    } else if !cell_data.flags.contains(CellFlags::INVERSE) && !is_default_bg(cell_data.bg) {
                        let bg_c = t.color_to_hsla(cell_data.bg);
                        if let Some(last) = bg_runs.last_mut() {
                            if last.col + last.len == c && hsla_eq(last.color, bg_c) {
                                last.len += 1;
                            } else {
                                bg_runs.push(BgRun {
                                    col: c,
                                    len: 1,
                                    color: bg_c,
                                });
                            }
                        } else {
                            bg_runs.push(BgRun {
                                col: c,
                                len: 1,
                                color: bg_c,
                            });
                        }
                    }

                    let cell_font = font_variants[font_idx].clone();

                    // Wide chars (CJK, hiragana, katakana, hangul,
                    // most emoji) still get their own segment so the
                    // shape_line call below can return a 2× advance
                    // glyph; mixing them with neighbour cells would
                    // confuse the `force_width` hint we pass to
                    // shape_line later (it forces every glyph to ONE
                    // cell width; a wide char needs two).
                    if is_wide {
                        if let Some(seg) = cur_seg.take() {
                            segments.push(seg);
                        }
                        segments.push(RawSegment {
                            col_start: c,
                            text: ch.to_string(),
                            runs: vec![TextRun {
                                len: ch.len_utf8(),
                                font: cell_font,
                                color: fg,
                                background_color: None,
                                underline,
                                strikethrough,
                            }],
                            cell_span: 2,
                        });
                        continue;
                    }

                    // Narrow cells flow into the running segment. The
                    // accent / CJK / block-element / dingbat drift
                    // (✽, ❯, ▏▎▌, é, è, …) is fixed at the
                    // shape_line call site below: gpui's 4th param
                    // `force_width: Option<Pixels>` clamps every
                    // glyph's advance to the supplied cell width, so
                    // regardless of what the fallback font reports
                    // for any individual glyph the run stays
                    // cell-aligned. Same pattern Zed's terminal_view
                    // uses (see crates/terminal_view/src/terminal_element.rs).
                    // Box-drawing glyphs from the font are usually narrower
                    // than the cell, so long `─` runs render dashed and
                    // column rules drift. Divert the common subset to
                    // geometric rendering (painted as connecting bars in
                    // Phase 2) and blank the glyph cell so the gappy font
                    // glyph isn't drawn underneath. `box_parts` was computed
                    // above (before the INVERSE swap) so the bar is painted in
                    // the normal foreground colour even for reverse-video cells.
                    if let Some(parts) = box_parts {
                        box_cells.push(BoxCell {
                            col: c,
                            parts,
                            color: fg,
                        });
                    }
                    let glyph_ch = if box_parts.is_some() { ' ' } else { ch };

                    let seg = cur_seg.get_or_insert_with(|| RawSegment {
                        col_start: c,
                        text: String::new(),
                        runs: Vec::new(),
                        cell_span: 1,
                    });
                    seg.text.push(glyph_ch);
                    let char_len = glyph_ch.len_utf8();
                    let can_merge = seg.runs.last().is_some_and(|last: &TextRun| {
                        last.color == fg
                            && last.font == cell_font
                            && last.underline == underline
                            && last.strikethrough == strikethrough
                    });
                    if let Some(last) = seg.runs.last_mut().filter(|_| can_merge) {
                        last.len += char_len;
                    } else {
                        seg.runs.push(TextRun {
                            len: char_len,
                            font: cell_font,
                            color: fg,
                            background_color: None,
                            underline,
                            strikethrough,
                        });
                    }
                }
                if let Some(seg) = cur_seg {
                    segments.push(seg);
                }

                *slot = Some(Rc::new(RawLine {
                    style_sig_memo: Cell::new(None),
                    abs_line,
                    text: full_text,
                    segments,
                    bg_runs,
                    box_cells,
                }));
            }

            // Save the working Vec for next frame BEFORE Phase 2
            // consumes it. The rows are `Rc`-shared, so this is one
            // refcount bump per row — no per-frame deep clone of text/
            // segments/runs while the Term lock is held.
            // Phase 1 fills every slot; `flatten` skips any stray `None` rather
            // than panicking (a missing row just isn't cached this frame).
            let raw_lines: Vec<Rc<RawLine>> = working.iter().flatten().cloned().collect();
            *self.prev_frame.borrow_mut() = Some(CachedFrame {
                display_offset,
                visible_cols,
                visible_lines,
                history_size,
                ring_len,
                lines: working,
            });

            let cursor = if cursor_visible
                && display_offset == 0
                && (cursor_point.line.0 as usize) < visible_lines
                && cursor_point.column.0 < visible_cols
            {
                Some((cursor_point.line.0 as usize, cursor_point.column.0))
            } else {
                None
            };

            let selection = term.selection.as_ref().and_then(|s| s.to_range(&*term));
            drop(term);

            (
                raw_lines,
                cursor,
                selection,
                visible_cols,
                display_offset as usize,
                history_size,
            )
        };
        // Lock released — event loop can proceed while we shape text.
        let phase1 = phase1_t0.elapsed();
        let phase2_t0 = std::time::Instant::now();

        // Phase 2: shape line segments (with cache) without holding the lock.
        let text_sys = window.text_system();
        let mut cache = self.line_cache.borrow_mut();
        // Build into last frame's (cleared) map and swap at the end —
        // steady-state zero HashMap allocations per frame.
        let mut new_cache = self.line_cache_scratch.borrow_mut();
        new_cache.clear();
        let mut result_lines = Vec::with_capacity(raw_lines.len());
        let mut detected: Vec<DetectedUrl> = Vec::new();
        for (line_idx, raw) in raw_lines.into_iter().enumerate() {
            let sig = raw.style_sig();
            if let Some(cached) = cache.remove(&raw.abs_line)
                && cached.text == raw.text
                && cached.sig == sig
            {
                // Gather cached URLs without re-running detection.
                for (start, end, url, is_file) in cached.urls.iter() {
                    detected.push(DetectedUrl {
                        line: line_idx,
                        start_col: *start,
                        end_col: *end,
                        url: url.clone(),
                        is_file: *is_file,
                    });
                }
                let segments = std::rc::Rc::clone(&cached.segments);
                new_cache.insert(raw.abs_line, cached);
                result_lines.push(TermLine {
                    // Cheap atomic bumps — no Vec/ShapedLine deep clone.
                    segments,
                    raw,
                });
                continue;
            }
            // Cache miss (damaged row) — the only path that still copies
            // the row's text/segments out of the shared `Rc<RawLine>`.
            let shaped_segments: Vec<TermSegment> = raw
                .segments
                .iter()
                .map(|seg| {
                    // 4th arg `force_width: Option<Pixels>` is gpui's
                    // terminal-cell-alignment hook: it clamps every
                    // glyph's advance to the supplied width regardless
                    // of what the font reports. Without it, a fallback
                    // glyph (✽ from Dingbats, é from Latin-Extended,
                    // ▏▎▌ from Block Elements, anything CJK before
                    // the WIDE_CHAR split kicks in) advances by its
                    // native width and every later cell in the segment
                    // drifts horizontally — visible as the "✽ spinner
                    // dancing up and down" + "ratatui bar columns
                    // misaligned" bugs. Same call shape Zed uses in
                    // `crates/terminal_view/src/terminal_element.rs`.
                    //
                    // Wide-char segments (CJK / 2-cell emoji ✅) span
                    // TWO grid cells, so the clamp must be 2 × the
                    // cell width — otherwise the emoji gets squashed
                    // into half its natural width and the spacer
                    // column next to it stays empty (the "split-
                    // rendered emoji" bug).
                    let force_w = cell.width * seg.cell_span as f32;
                    let shaped = text_sys.shape_line(seg.text.clone().into(), font_size, &seg.runs, Some(force_w));
                    TermSegment {
                        col_start: seg.col_start,
                        shaped,
                    }
                })
                .collect();
            let shaped_rc = std::rc::Rc::new(shaped_segments);
            let urls_rc = std::rc::Rc::new(
                detect_urls(&raw.text)
                    .into_iter()
                    .map(|(s, e, url, f)| (s, e, std::rc::Rc::<str>::from(url), f))
                    .collect::<Vec<_>>(),
            );
            for (start, end, url, is_file) in urls_rc.iter() {
                detected.push(DetectedUrl {
                    line: line_idx,
                    start_col: *start,
                    end_col: *end,
                    url: url.clone(),
                    is_file: *is_file,
                });
            }
            new_cache.insert(
                raw.abs_line,
                CachedLine {
                    text: raw.text.clone(),
                    segments: std::rc::Rc::clone(&shaped_rc),
                    urls: urls_rc,
                    sig,
                },
            );
            result_lines.push(TermLine {
                segments: shaped_rc,
                raw,
            });
        }

        // `cache` still holds entries for rows that vanished this frame;
        // they become next frame's scratch and are cleared on entry.
        std::mem::swap(&mut *cache, &mut *new_cache);
        *self.detected_urls.borrow_mut() = detected;

        let built = TermPrepaint {
            lines: result_lines,
            cursor,
            selection,
            visible_cols,
            display_offset: display_offset_val,
            history_size,
        };
        // Stash for the try-lock reuse path (cheap — TermLine is
        // Rc-backed, so the clone is refcount bumps, not glyph copies).
        *self.last_prepaint.borrow_mut() = Some(built.clone());
        paint_log::record(paint_log::Sample {
            total: paint_t0.elapsed(),
            phase1,
            phase2: phase2_t0.elapsed(),
        });
        Some(built)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // T3 — instrumentation: wrap paint with a wall-clock measurement.
        // The trace at the end shows BOTH the paint duration and a clear
        // marker for "frame committed", letting us compute T2 → T3
        // (echo bytes parsed → grid painted) — typically the segment
        // where lock contention with alacritty's EventLoop would show.
        // Enable with RUST_LOG=tab_atelier::input_lag=trace.
        let paint_started = std::time::Instant::now();

        let Some(state) = prepaint.take() else {
            return;
        };
        let cell = self.cell_size;
        let origin = bounds.origin;

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Opaque base fill so default-bg cells overwrite the previous
            // frame. Per-cell bg quads (below) skip default-bg cells —
            // without this base, anything left in the framebuffer from the
            // last redraw (other windows, scrolled-off rows) shows through.
            let term_bg = theme::theme(self.theme).term_bg_hsla();
            window.paint_quad(fill(bounds, term_bg));

            // Paint backgrounds, with vertically-contiguous identical
            // runs merged into single quads (see `merge_bg_rects`).
            for r in merge_bg_rects(state.lines.iter().map(|l| l.raw.bg_runs.as_slice())) {
                let pos = point(
                    origin.x + cell.width * r.col as f32,
                    origin.y + cell.height * r.row as f32,
                );
                let size = size(cell.width * r.len as f32, cell.height * r.rows as f32);
                window.paint_quad(fill(Bounds::new(pos, size), r.color));
            }

            // Paint selection. Selection coordinates are in alacritty's
            // grid Line space, where Line(0) is the top of the un-scrolled
            // viewport. Convert to the on-screen row by adding the current
            // display_offset (positive when the user has scrolled up into
            // scrollback), so the highlight tracks the actual content as it
            // moves under the user.
            if let Some(ref sel) = state.selection {
                let sel_color = Hsla::from(Rgba {
                    r: 0.2,
                    g: 0.4,
                    b: 0.7,
                    a: 0.5,
                });
                let start = sel.start;
                let end = sel.end;
                let off = state.display_offset as i32;
                for row in start.line.0..=end.line.0 {
                    let screen_row = row + off;
                    if screen_row < 0 || screen_row as usize >= state.lines.len() {
                        continue;
                    }
                    let col_start = if row == start.line.0 { start.column.0 } else { 0 };
                    let col_end = if row == end.line.0 {
                        end.column.0 + 1
                    } else {
                        state.visible_cols
                    };
                    if col_start >= col_end {
                        continue;
                    }
                    let pos = point(
                        origin.x + cell.width * col_start as f32,
                        origin.y + cell.height * screen_row as f32,
                    );
                    let sz = size(cell.width * (col_end - col_start) as f32, cell.height);
                    window.paint_quad(fill(Bounds::new(pos, sz), sel_color));
                }
            }

            // Paint text segments at grid-aligned positions.
            for (line_idx, line) in state.lines.iter().enumerate() {
                for seg in line.segments.iter() {
                    let pos = point(
                        origin.x + cell.width * seg.col_start as f32,
                        origin.y + cell.height * line_idx as f32,
                    );
                    let _ = seg.shaped.paint(pos, cell.height, window, cx);
                }
            }

            // Paint geometric box-drawing bars. These cells were blanked
            // in the glyph layer above; drawing them as filled bars that
            // reach the cell edges makes table rules connect instead of
            // showing the font's narrower, gappy glyph.
            let cw = f32::from(cell.width);
            let chh = f32::from(cell.height);
            for (line_idx, line) in state.lines.iter().enumerate() {
                let cell_y = origin.y + cell.height * line_idx as f32;
                let cells = &line.raw.box_cells;
                let mut i = 0;
                while i < cells.len() {
                    let bc = &cells[i];
                    // Pure horizontals (`─`, `━`, dashed forms) each
                    // paint one full-width bar, so a contiguous
                    // same-glyph same-colour run merges into a single
                    // quad — a table rule across 200 columns was 400
                    // `paint_quad` calls per frame, now it's 1. Corners
                    // / tees / crosses have vertical ink and stay on
                    // the per-cell path below.
                    if let Some(bar) = crate::box_drawing::h_bar(bc.parts, cw, chh) {
                        let mut end = i + 1;
                        while end < cells.len() {
                            let next = &cells[end];
                            if next.col != cells[end - 1].col + 1
                                || next.parts != bc.parts
                                || !hsla_eq(next.color, bc.color)
                            {
                                break;
                            }
                            end += 1;
                        }
                        let run_cols = cells[end - 1].col - bc.col + 1;
                        let pos = point(origin.x + cell.width * bc.col as f32, cell_y + px(bar.y));
                        let sz = size(px(cw * run_cols as f32), px(bar.h));
                        window.paint_quad(fill(Bounds::new(pos, sz), bc.color));
                        i = end;
                        continue;
                    }
                    let cell_x = origin.x + cell.width * bc.col as f32;
                    let (bars, n) = crate::box_drawing::rects(bc.parts, cw, chh);
                    for r in &bars[..n] {
                        let pos = point(cell_x + px(r.x), cell_y + px(r.y));
                        let sz = size(px(r.w), px(r.h));
                        window.paint_quad(fill(Bounds::new(pos, sz), bc.color));
                    }
                    i += 1;
                }
            }

            // Paint URL underlines on hover.
            let urls = self.detected_urls.borrow();
            if let Some((h_line, h_col)) = self.hover_grid.get() {
                let hovered_url = urls
                    .iter()
                    .find(|u| u.line == h_line && h_col >= u.start_col && h_col < u.end_col);
                if let Some(url) = hovered_url {
                    let underline_color = Hsla::from(Rgba {
                        r: 0.22,
                        g: 0.58,
                        b: 1.0,
                        a: 0.9,
                    });
                    let y = origin.y + cell.height * (url.line as f32 + 1.0) - px(2.0);
                    let x = origin.x + cell.width * url.start_col as f32;
                    let w = cell.width * (url.end_col - url.start_col) as f32;
                    window.paint_quad(fill(Bounds::new(point(x, y), size(w, px(1.0))), underline_color));
                }
            }
            drop(urls);

            // Paint cursor.
            if let Some((row, col)) = state.cursor {
                let pos = point(origin.x + cell.width * col as f32, origin.y + cell.height * row as f32);
                let cursor_size = size(cell.width, cell.height);
                window.paint_quad(fill(
                    Bounds::new(pos, cursor_size),
                    Hsla::from(Rgba {
                        r: 0.86,
                        g: 0.86,
                        b: 0.86,
                        a: 0.7,
                    }),
                ));
            }

            // Paint scrollbar.
            if state.history_size > 0 {
                let sb_width = px(SCROLLBAR_WIDTH);
                let track_left = origin.x + bounds.size.width - sb_width;
                let track_height = bounds.size.height;

                let track_bounds = Bounds::new(point(track_left, origin.y), size(sb_width, track_height));
                window.paint_quad(fill(
                    track_bounds,
                    Hsla::from(Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.05,
                    }),
                ));

                let total = (state.history_size + state.lines.len()) as f32;
                let visible_frac = state.lines.len() as f32 / total;
                let thumb_h = (visible_frac * f32::from(track_height)).max(20.0);
                let thumb_h = px(thumb_h);
                let max_offset = state.history_size as f32;
                let scroll_frac = 1.0 - (state.display_offset as f32 / max_offset);
                let available = track_height - thumb_h;
                let thumb_top = origin.y + available * scroll_frac;

                let thumb_bounds = Bounds::new(point(track_left, thumb_top), size(sb_width, thumb_h));
                let thumb_color = if state.display_offset > 0 {
                    Hsla::from(Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.4,
                    })
                } else {
                    Hsla::from(Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.2,
                    })
                };
                window.paint_quad(PaintQuad {
                    bounds: thumb_bounds,
                    corner_radii: Corners::all(px(3.0)),
                    background: thumb_color.into(),
                    border_widths: Edges::default(),
                    border_color: gpui::transparent_black(),
                    border_style: gpui::BorderStyle::default(),
                });
            }
        });

        let present = paint_started.elapsed();
        paint_log::record_present(present);
        trace!(
            target: crate::INPUT_TRACE_TARGET,
            "T3 paint done in {present:?}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FontConfig;
    use crate::term_export::sgr_color;
    use gpui::TestAppContext;
    use vte::ansi::{Color, NamedColor};

    fn default_browser() -> Rc<RefCell<Option<String>>> {
        Rc::new(RefCell::new(None))
    }

    fn default_editor() -> Rc<RefCell<Option<String>>> {
        Rc::new(RefCell::new(None))
    }

    fn frame(display_offset: i32, history_size: usize, ring_len: u64) -> CachedFrame {
        CachedFrame {
            display_offset,
            visible_cols: 80,
            visible_lines: 24,
            history_size,
            ring_len,
            lines: Vec::new(),
        }
    }

    /// A stationary window (offset AND scrollback length unchanged)
    /// reuses rows index-for-index — `Some(0)`, damage applied by the
    /// caller — and any resize rebuilds outright.
    #[test]
    fn cached_frame_stationary_and_resize() {
        let f = frame(0, 100, 7);
        assert_eq!(f.shift_for(0, 80, 24, 100, 7), Some(0));
        // Same window, output arrived in place (TUI redraw): still
        // index-stable; the damage rows handle the changed content.
        assert_eq!(f.shift_for(0, 80, 24, 100, 9), Some(0));
        assert_eq!(f.shift_for(0, 100, 24, 100, 7), None, "cols change (resize)");
        assert_eq!(f.shift_for(0, 80, 50, 100, 7), None, "lines change (resize)");
    }

    /// Guards the stale-bg-bleed fix, now in shift form: when output
    /// arrives AND the window moves (streaming grows `history_size`
    /// while `display_offset` stays 0), the index-keyed rows must not
    /// be reused as-is — the old bug served a stale inverse-red row
    /// under whatever was drawn there next. With bytes in flight the
    /// shift is not trustworthy either (the scrollback ring may be
    /// saturated and rotating), so the frame rebuilds.
    #[test]
    fn cached_frame_stream_scroll_rebuilds() {
        let f = frame(0, 100, 7);
        assert_eq!(f.shift_for(0, 80, 24, 101, 8), None, "history grew + output");
        assert_eq!(f.shift_for(0, 80, 24, 0, 8), None, "\\x1b[3J history clear");
        assert_eq!(f.shift_for(5, 80, 24, 100, 8), None, "user scroll during flood");
    }

    /// A pure user scroll — no PTY bytes since the cache was built —
    /// realigns the cached rows instead of rebuilding them: slot `i`
    /// takes the row from old slot `i + shift`, where the shift is the
    /// change of the content-stable base `history_size - display_offset`.
    #[test]
    fn cached_frame_pure_scroll_shifts() {
        let f = frame(0, 100, 7);
        // Scrolling up 5 lines: content moves DOWN the screen.
        assert_eq!(f.shift_for(5, 80, 24, 100, 7), Some(-5));
        // And back toward the bottom from offset 5.
        assert_eq!(frame(5, 100, 7).shift_for(2, 80, 24, 100, 7), Some(3));
        // Scrolled all the way with a full 10k scrollback: still a
        // plain shift — history must never be truncated to avoid this.
        assert_eq!(frame(0, 10_000, 7).shift_for(10_000, 80, 24, 10_000, 7), Some(-10_000));
    }

    /// The realignment itself: `None` slots are the rows the cell scan
    /// rebuilds, everything else is carried over in place.
    #[test]
    fn shift_rows_realigns_and_exposes() {
        let mut v: Vec<Option<u32>> = vec![Some(0), Some(1), Some(2), Some(3)];
        // Content moved up 2 (scrolled toward the bottom): slot 0 shows
        // what slot 2 held; the 2 newly exposed bottom slots rebuild.
        shift_rows(&mut v, 2);
        assert_eq!(v, vec![Some(2), Some(3), None, None]);
        let mut v: Vec<Option<u32>> = vec![Some(0), Some(1), Some(2), Some(3)];
        // Scrolled up 1: content moves down, top slot rebuilds.
        shift_rows(&mut v, -1);
        assert_eq!(v, vec![None, Some(0), Some(1), Some(2)]);
        // A jump past the whole window exposes everything.
        let mut v: Vec<Option<u32>> = vec![Some(0), Some(1)];
        shift_rows(&mut v, 40);
        assert_eq!(v, vec![None, None]);
        let mut v: Vec<Option<u32>> = vec![Some(0), Some(1)];
        shift_rows(&mut v, -40);
        assert_eq!(v, vec![None, None]);
    }

    fn run(col: usize, len: usize, l: f32) -> BgRun {
        BgRun {
            col,
            len,
            color: Hsla {
                h: 0.6,
                s: 0.5,
                l,
                a: 1.0,
            },
        }
    }

    /// A TUI block painting the same run on consecutive rows must melt
    /// into ONE rectangle spanning them; anything that differs (column,
    /// width, colour) or skips a row starts a fresh rect.
    #[test]
    fn bg_rects_merge_vertically_and_break_correctly() {
        // Three identical rows + one row with a different colour.
        let rows: Vec<Vec<BgRun>> = vec![
            vec![run(2, 10, 0.3)],
            vec![run(2, 10, 0.3)],
            vec![run(2, 10, 0.3)],
            vec![run(2, 10, 0.9)],
        ];
        let mut rects = merge_bg_rects(rows.iter().map(Vec::as_slice));
        rects.sort_by_key(|r| r.row);
        assert_eq!(rects.len(), 2, "3-row span + 1 colour-break rect");
        assert_eq!((rects[0].row, rects[0].rows), (0, 3));
        assert_eq!((rects[1].row, rects[1].rows), (3, 1));

        // A gap row breaks the span; different col/len never merge.
        let rows: Vec<Vec<BgRun>> = vec![
            vec![run(0, 4, 0.3), run(8, 2, 0.3)],
            vec![],
            vec![run(0, 4, 0.3), run(8, 3, 0.3)],
        ];
        let rects = merge_bg_rects(rows.iter().map(Vec::as_slice));
        assert_eq!(rects.len(), 4, "gap + width change: nothing merges");
        assert!(rects.iter().all(|r| r.rows == 1));

        // Independent columns merge independently.
        let rows: Vec<Vec<BgRun>> = vec![
            vec![run(0, 4, 0.3), run(8, 2, 0.5)],
            vec![run(0, 4, 0.3), run(8, 2, 0.5)],
        ];
        let mut rects = merge_bg_rects(rows.iter().map(Vec::as_slice));
        rects.sort_by_key(|r| r.col);
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|r| r.rows == 2), "both columns span both rows");

        assert!(merge_bg_rects(std::iter::empty()).is_empty());
    }

    /// The per-row style signature is memoised on the shared `RawLine` —
    /// it must equal the free-function result and stay stable across
    /// calls (the whole point: cache hits stop re-walking the runs).
    #[test]
    fn raw_line_style_sig_is_memoised_and_consistent() {
        let segments = vec![RawSegment {
            col_start: 0,
            text: "hello".to_string(),
            runs: vec![TextRun {
                len: 5,
                font: font("monospace"),
                color: Hsla {
                    h: 0.1,
                    s: 0.2,
                    l: 0.3,
                    a: 1.0,
                },
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            cell_span: 1,
        }];
        let raw = RawLine {
            style_sig_memo: Cell::new(None),
            abs_line: 7,
            text: "hello".into(),
            segments,
            bg_runs: Vec::new(),
            box_cells: Vec::new(),
        };
        let expect = line_style_sig(&raw.segments);
        assert_eq!(raw.style_sig(), expect, "memo equals the direct computation");
        assert_eq!(raw.style_sig(), expect, "second call serves the memo");
        assert_eq!(raw.style_sig_memo.get(), Some(expect), "memo populated");
    }

    /// Guards the "pasted text is invisible until you edit it" fix. Readline
    /// highlights a bracketed paste with reverse-video, then re-emits the SAME
    /// characters without reverse the moment the region deactivates. The
    /// Phase-2 glyph cache is keyed on `abs_line` + text; before the fix it
    /// reused the reverse-shaped (background-coloured) glyphs on that
    /// identical-text redraw. `line_style_sig` must differ when only the
    /// colour changes so the redraw misses the cache and re-shapes.
    #[test]
    fn line_style_sig_tracks_colour_not_just_text() {
        let mk = |color: Hsla| {
            vec![RawSegment {
                col_start: 0,
                text: "/mnt/Dev".to_string(),
                runs: vec![TextRun {
                    len: 8,
                    font: font("monospace"),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                cell_span: 1,
            }]
        };
        let white = Hsla::from(Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        });
        let navy = Hsla::from(Rgba {
            r: 0.0,
            g: 0.14,
            b: 0.32,
            a: 1.0,
        });
        // Same text, different foreground → different signature (cache miss).
        assert_ne!(line_style_sig(&mk(white)), line_style_sig(&mk(navy)));
        // Identical inputs → identical signature (cache hit, no re-shape).
        assert_eq!(line_style_sig(&mk(white)), line_style_sig(&mk(white)));
    }

    #[gpui::test]
    fn test_terminal_view_creation(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                assert!(view.pid() > 0);
                assert!(!view.has_exited());
                assert!(view.last_input_time().is_none());
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_restore_output(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.restore_output("hello world\nsecond line");
                let t = view.term.lock();
                let grid = t.grid();
                let mut found = false;
                for row in 0..grid.screen_lines() as i32 {
                    let mut line_text = String::new();
                    for col in 0..grid.columns() {
                        let cell = &grid[GridPoint::new(Line(row), Column(col))];
                        line_text.push(cell.c);
                    }
                    if line_text.contains("hello world") {
                        found = true;
                        break;
                    }
                }
                assert!(found, "restored text should appear in grid");
                drop(t);
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn force_resize_sizes_a_background_view(cx: &mut TestAppContext) {
        // A background tab is never painted, so it relies on `force_resize`
        // (driven from the app's broadcast tick) to match the active tab —
        // resizing the grid + PTY and reporting the new size via measured_grid.
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });
        window
            .update(cx, |view, _window, _cx| {
                let cell = Size {
                    width: px(8.0),
                    height: px(16.0),
                };
                view.force_resize(120, 40, cell);
                assert_eq!(view.dims(), (120, 40), "the PTY grid resized");
                let (mc, ml, _) = view.measured_grid().expect("size reported after resize");
                assert_eq!((mc, ml), (120, 40), "measured cols/lines match");
                // Idempotent — re-applying the same size keeps the grid.
                view.force_resize(120, 40, cell);
                assert_eq!(view.dims(), (120, 40));
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn skeleton_tab_defers_shell_until_ensure_spawned(cx: &mut TestAppContext) {
        // A background tab is built as a skeleton (`defer_spawn = true`): the
        // view exists and renders, but no shell is forked until `ensure_spawned`
        // — that's what keeps startup from blocking on ~60 forks.
        let window = cx.add_window(|window, cx| {
            TerminalView::new_with_colors_and_env(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                true,
                std::collections::HashMap::new(),
                None,
                None,
                true, // defer_spawn — skeleton
                window,
                cx,
            )
        });
        window
            .update(cx, |view, _window, _cx| {
                assert!(!view.is_spawned(), "skeleton hasn't forked a shell");
                assert_eq!(view.pid(), 0, "no pid before spawn");
            })
            .unwrap();
        window
            .update(cx, |view, _window, _cx| {
                view.ensure_spawned();
                assert!(view.is_spawned(), "shell forked after ensure_spawned");
                assert!(view.pid() > 0, "a real pid now");
                // Idempotent — a second call is a no-op.
                let pid = view.pid();
                view.ensure_spawned();
                assert_eq!(view.pid(), pid);
                view.shutdown();
            })
            .unwrap();
    }

    /// The resize-storm rate limit: a parked size only reflows the grid
    /// once it has been stable for `RESIZE_SETTLE`, and the reflow goes
    /// through `force_resize` (grid actually changes size).
    #[gpui::test]
    fn pending_resize_applies_only_after_settling(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new_with_colors_and_env(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                true,
                std::collections::HashMap::new(),
                None,
                Some((80, 24, size(px(8.0), px(16.0)))),
                true, // skeleton — no shell needed to resize the grid
                window,
                cx,
            )
        });
        window
            .update(cx, |view, _window, _cx| {
                // The test window's first paint applies its own size —
                // work relative to whatever the grid is now.
                let base = view.dims();
                let target = (usize::from(base.0) + 7, usize::from(base.1) + 3);
                // Mid-storm: request too fresh — nothing applies.
                view.pending_resize
                    .set(Some((target.0, target.1, std::time::Instant::now())));
                assert!(!view.apply_pending_resize(), "still settling");
                assert_eq!(view.dims(), base, "grid untouched mid-storm");
                // Settled: same request, aged past RESIZE_SETTLE.
                let old = std::time::Instant::now().checked_sub(RESIZE_SETTLE).unwrap();
                view.pending_resize.set(Some((target.0, target.1, old)));
                assert!(view.apply_pending_resize(), "settled — reflow happens");
                assert_eq!(
                    (usize::from(view.dims().0), usize::from(view.dims().1)),
                    target,
                    "grid reflowed to the parked size"
                );
                assert!(view.pending_resize.get().is_none(), "pending consumed");
                // Idempotent once consumed.
                assert!(!view.apply_pending_resize());
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_restore_output_empty(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.restore_output("");
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_send_input_updates_last_input(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                assert!(view.last_input_time().is_none());
                view.send_input(b"hello".to_vec());
                assert!(view.last_input_time().is_some());
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_send_clipboard_plain(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.send_clipboard("pasted text");
                assert!(view.last_input_time().is_some());
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_scroll(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                // Write enough newlines via the parser to overflow the 24-line screen
                let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
                let mut term = view.term.lock();
                for _ in 0..200 {
                    parser.advance(&mut *term, b"x\n");
                }
                drop(parser);
                let history = term.grid().history_size();
                drop(term);
                assert!(history > 0, "should have scroll history after 200 lines");

                view.scroll(5);
                let offset = view.term.lock().grid().display_offset();
                assert!(offset > 0, "scroll up should increase offset");
                view.scroll(-5);
                let offset2 = view.term.lock().grid().display_offset();
                assert!(offset2 < offset, "scroll down should decrease offset");
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_scroll_to_fraction(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.restore_output(&"line\n".repeat(100));
                view.scroll_to_fraction(0.0);
                let top = view.term.lock().grid().display_offset();
                view.scroll_to_fraction(1.0);
                let bottom = view.term.lock().grid().display_offset();
                assert!(top > bottom);
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_selection(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                // Clear + home first so the seeded text lands at (0,0)
                // deterministically — the live shell forked by `new` emits its
                // prompt (and a startup newline) asynchronously, which would
                // otherwise race the cursor.
                view.restore_output("\x1b[2J\x1b[Hselect this text");
                assert!(!view.has_selection(), "no selection yet");
                let start = GridPoint::new(Line(0), Column(0));
                let end = GridPoint::new(Line(0), Column(5));
                view.start_selection(start, Side::Left);
                view.update_selection(end, Side::Right);
                assert!(view.has_selection(), "cheap probe sees the selection");
                let text = view.copy_selection();
                assert!(text.is_some());
                assert!(!text.unwrap().is_empty());
                view.clear_selection();
                assert!(view.copy_selection().is_none());
                assert!(!view.has_selection(), "probe clears with the selection");
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_reset_terminal(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.reset_terminal();
                view.shutdown();
            })
            .unwrap();
    }

    #[gpui::test]
    fn test_copy_all_history(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| {
            TerminalView::new(
                None,
                FontConfig::default(),
                default_browser(),
                default_editor(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, _window, _cx| {
                view.restore_output("first line\nsecond line");
                let history = view.copy_all_history();
                assert!(history.contains("first line"));
                assert!(history.contains("second line"));
                view.shutdown();
            })
            .unwrap();
    }

    #[test]
    fn test_sgr_color_named() {
        let mut sgr = String::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::Red), true);
        assert_eq!(sgr, "31");

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::Red), false);
        assert_eq!(sgr, "41");
    }

    #[test]
    fn test_sgr_color_bright() {
        let mut sgr = String::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::BrightRed), true);
        assert_eq!(sgr, "91");

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::BrightRed), false);
        assert_eq!(sgr, "101");
    }

    #[test]
    fn test_sgr_color_foreground_default() {
        let mut sgr = String::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::Foreground), true);
        assert_eq!(sgr, "39");

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::Foreground), false);
        assert_eq!(sgr, "49");
    }

    #[test]
    fn test_sgr_color_indexed() {
        let mut sgr = String::new();
        sgr_color(&mut sgr, Color::Indexed(196), true);
        assert_eq!(sgr, "38;5;196");

        sgr.clear();
        sgr_color(&mut sgr, Color::Indexed(196), false);
        assert_eq!(sgr, "48;5;196");
    }

    #[test]
    fn test_sgr_color_rgb() {
        let mut sgr = String::new();
        sgr_color(&mut sgr, Color::Spec(vte::ansi::Rgb { r: 255, g: 128, b: 0 }), true);
        assert_eq!(sgr, "38;2;255;128;0");
    }

    #[test]
    fn test_sgr_color_appends_semicolon() {
        let mut sgr = String::from("1");
        sgr_color(&mut sgr, Color::Named(NamedColor::Red), true);
        assert_eq!(sgr, "1;31");
    }

    #[test]
    fn test_visible_lines_default() {
        // Without cell_size set, should return 25
        // We can't easily construct a TerminalView without gpui context,
        // so this is tested indirectly via the scroll tests.
    }
}
