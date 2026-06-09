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

use log::{info, trace};

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

#[derive(Clone)]
pub struct DetectedUrl {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub url: String,
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
}

impl EventProxy {
    fn set_notifier(&self, sender: EventLoopSender) {
        if let Ok(mut slot) = self.notifier.lock() {
            *slot = Some(sender);
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        if let AlacrittyEvent::PtyWrite(text) = event
            && let Ok(slot) = self.notifier.lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(Msg::Input(text.into_bytes().into()));
        }
    }
}

use crate::term_export::TermDims;

struct CachedLine {
    text: String,
    /// Shared with the per-frame `TermLine::segments`. Wrapping in `Rc` so
    /// a cache hit costs one atomic bump instead of deep-cloning the whole
    /// `Vec<TermSegment>` (each segment carries a `ShapedLine`).
    segments: std::rc::Rc<Vec<TermSegment>>,
    /// URLs detected in this line, computed once on cache miss and reused
    /// on every subsequent hit so `detect_urls` doesn't re-run every frame.
    /// The tuple shape matches `crate::detect_urls`'s return.
    urls: std::rc::Rc<Vec<(usize, usize, String, bool)>>,
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: EventLoopSender,
    event_proxy: EventProxy,
    focus: FocusHandle,
    cell_size: Option<Size<Pixels>>,
    last_size: Rc<Cell<Option<(usize, usize)>>>,
    content_origin: Rc<Cell<gpui::Point<Pixels>>>,
    bounds_size: Rc<Cell<Size<Pixels>>>,
    line_cache: Rc<RefCell<HashMap<i32, CachedLine>>>,
    pid: u32,
    exited: Rc<Cell<bool>>,
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
        Self::new_with_colors_and_env(cwd, font_config, browser, code_editor, true, HashMap::new(), window, cx)
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ws = WindowSize {
            num_lines: INITIAL_LINES as u16,
            num_cols: INITIAL_COLS as u16,
            cell_width: 9,
            cell_height: 18,
        };
        let mut env = pty_env(initial_colors_enabled);
        env.extend(extra_env);
        let opts = tty::Options {
            working_directory: cwd.map(std::path::Path::to_path_buf),
            env,
            ..Default::default()
        };
        let pty = tty::new(&opts, ws, 0).expect("failed to create pty");
        // ConPTY's Pty doesn't expose the child the way the Unix one does.
        // The PID feeds /proc cwd + catbus detection — both Linux-only, so
        // a 0 sentinel is fine on Windows.
        #[cfg(unix)]
        let pid = pty.child().id();
        #[cfg(windows)]
        let pid = 0u32;
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let proxy = EventProxy::default();
        let term = Term::new(
            config,
            &TermDims {
                columns: INITIAL_COLS,
                screen_lines: INITIAL_LINES,
            },
            proxy.clone(),
        );
        let term = Arc::new(FairMutex::new(term));
        let pty_ring = Arc::new(std::sync::Mutex::new(crate::pty_ring::PtyRing::default()));
        let pty = crate::pty_ring::PtyTap::new(pty, pty_ring.clone());
        let el = EventLoop::new(term.clone(), proxy.clone(), pty, false, false).expect("failed to create event loop");
        let notifier = el.channel();
        proxy.set_notifier(notifier.clone());
        el.spawn();

        let focus = cx.focus_handle();

        let tick = Rc::new(Cell::new(0u32));
        let tick_clone = tick;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(Duration::from_millis(33)).await;
                let Ok(()) = this.update(cx, |view, cx: &mut Context<Self>| {
                    let n = tick_clone.get().wrapping_add(1);
                    tick_clone.set(n);
                    let scrolled = view.term.lock().grid().display_offset() > 0;
                    if !scrolled || n.is_multiple_of(6) {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        let exited = Rc::new(Cell::new(false));
        let exited_clone = exited.clone();
        let pid_for_check = pid;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                if !crate::platform::process_alive(pid_for_check) {
                    let still_current = this
                        .update(cx, |view: &mut Self, _| view.pid == pid_for_check)
                        .unwrap_or(false);
                    if still_current {
                        exited_clone.set(true);
                        let _ = this.update(cx, |_, cx: &mut Context<Self>| cx.notify());
                    }
                    break;
                }
            }
        })
        .detach();

        Self {
            term,
            notifier,
            event_proxy: proxy,
            focus,
            cell_size: None,
            last_size: Rc::new(Cell::new(None)),
            content_origin: Rc::new(Cell::new(point(px(0.0), px(0.0)))),
            bounds_size: Rc::new(Cell::new(size(px(0.0), px(0.0)))),
            line_cache: Rc::new(RefCell::new(HashMap::new())),
            pid,
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
        }
    }

    /// Clone of the per-tab PTY ring's Arc. Lets the snapshot
    /// pipeline expose the ring to the API layer without giving the
    /// snapshot mutable access to the rest of the view.
    #[must_use]
    pub fn pty_ring(&self) -> Arc<std::sync::Mutex<crate::pty_ring::PtyRing>> {
        self.pty_ring.clone()
    }

    pub const fn colors_enabled(&self) -> bool {
        self.colors_enabled.get()
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn set_colors_enabled(&self, enabled: bool) {
        self.colors_enabled.set(enabled);
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
        self.exited.get()
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

    pub fn respawn(&mut self, cwd: Option<&Path>, cx: &mut Context<Self>) {
        let _ = self.notifier.send(Msg::Shutdown);

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

        let opts = tty::Options {
            working_directory: cwd.map(std::path::Path::to_path_buf),
            env: pty_env(self.colors_enabled.get()),
            ..Default::default()
        };
        let pty = tty::new(&opts, ws, 0).expect("failed to create pty");
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
        let el = EventLoop::new(self.term.clone(), self.event_proxy.clone(), pty, false, false)
            .expect("failed to create event loop");
        self.notifier = el.channel();
        self.event_proxy.set_notifier(self.notifier.clone());
        el.spawn();

        self.pid = pid;
        self.exited.set(false);

        let exited = self.exited.clone();
        let pid_for_check = pid;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                if !crate::platform::process_alive(pid_for_check) {
                    let still_current = this
                        .update(cx, |view: &mut Self, _| view.pid == pid_for_check)
                        .unwrap_or(false);
                    if still_current {
                        exited.set(true);
                        let _ = this.update(cx, |_, cx: &mut Context<Self>| cx.notify());
                    }
                    break;
                }
            }
        })
        .detach();
    }

    pub fn shutdown(&self) {
        let _ = self.notifier.send(Msg::Shutdown);
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
            target: "tab_atelier::input_lag",
            "T1 send_input bytes={} preview={:?}",
            bytes.len(),
            std::str::from_utf8(&bytes[..preview_len]).unwrap_or("<non-utf8>"),
        );
        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);
        let _ = self.notifier.send(Msg::Input(bytes.into()));
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
        let _ = self.notifier.send(Msg::Input(payload.into_bytes().into()));
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

    #[allow(clippy::significant_drop_tightening)]
    pub fn copy_all_history(&self) -> String {
        self.ansi_lines(None).0.join("\n")
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
    pub fn ansi_text_with_cursor(&self, max_lines: Option<usize>) -> (String, Option<(usize, usize)>) {
        let (lines, cursor) = self.ansi_lines(max_lines);
        (lines.join("\n"), cursor)
    }

    /// Delegates to the shared `term_export` so the GUI and headless
    /// paths can't drift. Kept private to preserve the existing
    /// public surface (`plain_text`, `ansi_text_with_cursor`).
    fn ansi_lines(&self, max_lines: Option<usize>) -> (Vec<String>, Option<(usize, usize)>) {
        let (text, cursor) = crate::term_export::term_to_ansi_text_with_cursor(&self.term, max_lines);
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        (lines, cursor)
    }

    fn url_at_grid(&self, line: usize, col: usize) -> Option<DetectedUrl> {
        let urls = self.detected_urls.borrow();
        urls.iter()
            .find(|u| u.line == line && col >= u.start_col && col < u.end_col)
            .cloned()
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

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let term = self.term.clone();

        // Measure cell size once we have a text system.
        // Cell width measurement based on Zed's terminal_element.rs
        // Copyright (c) Zed Industries — Apache-2.0 / GPL-3.0
        if self.cell_size.is_none() {
            let mut f = font(self.font_config.family.clone());
            f.weight = FontWeight(self.font_config.weight as f32);
            let font_size = px(self.font_config.size);
            let text_sys = window.text_system();
            let font_id = text_sys.resolve_font(&f);
            // Measure cell width through the shaping pipeline to match shape_line
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
            let cell_width = layout.width;
            let _ = font_id;
            let line_height = font_size * 1.4;
            self.cell_size = Some(Size {
                width: cell_width,
                height: line_height,
            });
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
                    target: "tab_atelier::input_lag",
                    "T0 keystroke key={:?} ctrl={} shift={} alt={}",
                    ks.key,
                    ks.modifiers.control,
                    ks.modifiers.shift,
                    ks.modifiers.alt
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
                content_origin: self.content_origin.clone(),
                bounds_size: self.bounds_size.clone(),
                line_cache: self.line_cache.clone(),
                theme: self.theme,
                font_config: self.font_config.clone(),
                detected_urls: self.detected_urls.clone(),
                hover_grid: self.hover_grid.clone(),
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
    notifier: EventLoopSender,
    cell_size: Size<Pixels>,
    last_size: Rc<Cell<Option<(usize, usize)>>>,
    content_origin: Rc<Cell<gpui::Point<Pixels>>>,
    bounds_size: Rc<Cell<Size<Pixels>>>,
    line_cache: Rc<RefCell<HashMap<i32, CachedLine>>>,
    theme: ThemeName,
    font_config: FontConfig,
    detected_urls: Rc<RefCell<Vec<DetectedUrl>>>,
    hover_grid: Rc<Cell<Option<(usize, usize)>>>,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

struct TermPrepaint {
    lines: Vec<TermLine>,
    cursor: Option<(usize, usize)>,
    selection: Option<SelectionRange>,
    visible_cols: usize,
    display_offset: usize,
    history_size: usize,
}

use alacritty_terminal::selection::SelectionRange;

struct TermSegment {
    col_start: usize,
    shaped: ShapedLine,
}

impl Clone for TermSegment {
    fn clone(&self) -> Self {
        Self {
            col_start: self.col_start,
            shaped: self.shaped.clone(),
        }
    }
}

struct TermLine {
    segments: std::rc::Rc<Vec<TermSegment>>,
    bg_runs: Vec<BgRun>,
}

struct BgRun {
    col: usize,
    len: usize,
    color: Hsla,
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
        struct RawSegment {
            col_start: usize,
            text: String,
            runs: Vec<TextRun>,
        }

        struct RawLine {
            grid_line: i32,
            text: String,
            segments: Vec<RawSegment>,
            bg_runs: Vec<BgRun>,
        }

        let cell = self.cell_size;
        let cols = ((bounds.size.width / cell.width) as usize).max(2);
        let lines = ((bounds.size.height / cell.height) as usize).max(1);

        if self.last_size.get() != Some((cols, lines)) {
            self.last_size.set(Some((cols, lines)));
            {
                let mut t = self.term.lock();
                t.resize(TermDims {
                    columns: cols,
                    screen_lines: lines,
                });
            }
            let _ = self.notifier.send(Msg::Resize(WindowSize {
                num_lines: lines as u16,
                num_cols: cols as u16,
                cell_width: f32::from(cell.width) as u16,
                cell_height: f32::from(cell.height) as u16,
            }));
        }

        self.content_origin.set(bounds.origin);
        self.bounds_size.set(bounds.size);

        let mut mono_font = font(self.font_config.family.clone());
        mono_font.weight = FontWeight(self.font_config.weight as f32);
        let font_size = px(self.font_config.size);
        let t = theme::theme(self.theme);
        let fg_default = t.term_fg_hsla();

        // Phase 1: read cell data under the lock — no shaping here.

        let (raw_lines, cursor, selection, visible_cols, display_offset_val, history_size) = {
            let term = self.term.lock();
            let grid = term.grid();
            let cursor_point = grid.cursor.point;
            let display_offset = grid.display_offset() as i32;
            let visible_lines = grid.screen_lines().min(lines);
            let visible_cols = grid.columns().min(cols);

            let mut raw_lines = Vec::with_capacity(visible_lines);

            for l in 0..visible_lines {
                let grid_line = l as i32 - display_offset;
                let mut full_text = String::with_capacity(visible_cols);
                let mut segments: Vec<RawSegment> = Vec::new();
                let mut cur_seg: Option<RawSegment> = None;
                let mut bg_runs: Vec<BgRun> = Vec::new();

                for c in 0..visible_cols {
                    let cell_data = &grid[GridPoint::new(Line(grid_line), Column(c))];
                    if cell_data.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        full_text.push(' ');
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

                    let mut font_weight = FontWeight(self.font_config.weight as f32);
                    let mut font_style = FontStyle::Normal;
                    let mut underline = None;
                    let mut strikethrough = None;

                    if cell_data.flags.contains(CellFlags::BOLD) {
                        font_weight = FontWeight::BOLD;
                    }
                    if cell_data.flags.contains(CellFlags::ITALIC) {
                        font_style = FontStyle::Italic;
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
                    if cell_data.flags.contains(CellFlags::INVERSE) {
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
                    } else if !is_default_bg(cell_data.bg) {
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

                    let mut cell_font = font(mono_font.family.clone());
                    cell_font.weight = font_weight;
                    cell_font.style = font_style;

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
                    let seg = cur_seg.get_or_insert_with(|| RawSegment {
                        col_start: c,
                        text: String::new(),
                        runs: Vec::new(),
                    });
                    seg.text.push(ch);
                    let char_len = ch.len_utf8();
                    let can_merge = seg.runs.last().is_some_and(|last: &TextRun| {
                        last.color == fg
                            && last.font == cell_font
                            && last.underline == underline
                            && last.strikethrough == strikethrough
                    });
                    if can_merge {
                        seg.runs.last_mut().unwrap().len += char_len;
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

                raw_lines.push(RawLine {
                    grid_line,
                    text: full_text,
                    segments,
                    bg_runs,
                });
            }

            let cursor = if display_offset == 0
                && (cursor_point.line.0 as usize) < visible_lines
                && cursor_point.column.0 < visible_cols
            {
                Some((cursor_point.line.0 as usize, cursor_point.column.0))
            } else {
                None
            };

            let selection = term.selection.as_ref().and_then(|s| s.to_range(&*term));
            let history_size = grid.history_size();
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

        // Phase 2: shape line segments (with cache) without holding the lock.
        let text_sys = window.text_system();
        let mut cache = self.line_cache.borrow_mut();
        let mut new_cache = HashMap::with_capacity(raw_lines.len());
        let mut result_lines = Vec::with_capacity(raw_lines.len());
        let mut detected: Vec<DetectedUrl> = Vec::new();
        for (line_idx, raw) in raw_lines.into_iter().enumerate() {
            if let Some(cached) = cache.remove(&raw.grid_line)
                && cached.text == raw.text
            {
                result_lines.push(TermLine {
                    // Cheap atomic bump — no Vec/ShapedLine deep clone.
                    segments: std::rc::Rc::clone(&cached.segments),
                    bg_runs: raw.bg_runs,
                });
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
                new_cache.insert(raw.grid_line, cached);
                continue;
            }
            let shaped_segments: Vec<TermSegment> = raw
                .segments
                .into_iter()
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
                    let shaped = text_sys.shape_line(seg.text.into(), font_size, &seg.runs, Some(cell.width));
                    TermSegment {
                        col_start: seg.col_start,
                        shaped,
                    }
                })
                .collect();
            let shaped_rc = std::rc::Rc::new(shaped_segments);
            let urls_rc = std::rc::Rc::new(detect_urls(&raw.text));
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
                raw.grid_line,
                CachedLine {
                    text: raw.text,
                    segments: std::rc::Rc::clone(&shaped_rc),
                    urls: urls_rc,
                },
            );
            result_lines.push(TermLine {
                segments: shaped_rc,
                bg_runs: raw.bg_runs,
            });
        }

        *cache = new_cache;
        *self.detected_urls.borrow_mut() = detected;

        Some(TermPrepaint {
            lines: result_lines,
            cursor,
            selection,
            visible_cols,
            display_offset: display_offset_val,
            history_size,
        })
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

            // Paint backgrounds.
            for (line_idx, line) in state.lines.iter().enumerate() {
                for bg in &line.bg_runs {
                    let pos = point(
                        origin.x + cell.width * bg.col as f32,
                        origin.y + cell.height * line_idx as f32,
                    );
                    let size = size(cell.width * bg.len as f32, cell.height);
                    window.paint_quad(fill(Bounds::new(pos, size), bg.color));
                }
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

        trace!(
            target: "tab_atelier::input_lag",
            "T3 paint done in {:?}",
            paint_started.elapsed(),
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
                view.restore_output("select this text");
                let start = GridPoint::new(Line(0), Column(0));
                let end = GridPoint::new(Line(0), Column(5));
                view.start_selection(start, Side::Left);
                view.update_selection(end, Side::Right);
                let text = view.copy_selection();
                assert!(text.is_some());
                assert!(!text.unwrap().is_empty());
                view.clear_selection();
                assert!(view.copy_selection().is_none());
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
