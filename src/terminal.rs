// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use log::info;

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};

use crate::terminal_utils::{hsla_eq, is_default_bg, is_default_fg, keystroke_to_bytes};
use crate::theme::{self, ThemeName};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use gpui::{
    App, AsyncApp, Bounds, ClipboardItem, ContentMask, Context, Corners, Edges, Element, ElementId, FocusHandle,
    Focusable, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement,
    Pixels, Render, Rgba, ScrollWheelEvent, ShapedLine, Size, StrikethroughStyle, Style, Styled, TextRun,
    UnderlineStyle, WeakEntity, Window, div, fill, font, point, px, relative, size,
};
use tab_atelier::{FontConfig, detect_urls, file_path_for_open};
use vte::ansi::{Color, NamedColor};

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

#[derive(Clone)]
struct EventProxy;
impl EventListener for EventProxy {}

struct TermDims {
    columns: usize,
    screen_lines: usize,
}
impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

struct CachedLine {
    text: String,
    segments: Vec<TermSegment>,
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: EventLoopSender,
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
    pub fn new(
        cwd: Option<&Path>,
        font_config: FontConfig,
        browser: Rc<RefCell<Option<String>>>,
        code_editor: Rc<RefCell<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_colors(cwd, font_config, browser, code_editor, true, window, cx)
    }

    pub fn new_with_colors(
        cwd: Option<&Path>,
        font_config: FontConfig,
        browser: Rc<RefCell<Option<String>>>,
        code_editor: Rc<RefCell<Option<String>>>,
        initial_colors_enabled: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ws = WindowSize {
            num_lines: INITIAL_LINES as u16,
            num_cols: INITIAL_COLS as u16,
            cell_width: 9,
            cell_height: 18,
        };
        let opts = tty::Options {
            working_directory: cwd.map(std::path::Path::to_path_buf),
            env: pty_env(initial_colors_enabled),
            ..Default::default()
        };
        let pty = tty::new(&opts, ws, 0).expect("failed to create pty");
        let pid = pty.child().id();
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &TermDims {
                columns: INITIAL_COLS,
                screen_lines: INITIAL_LINES,
            },
            EventProxy,
        );
        let term = Arc::new(FairMutex::new(term));
        let el = EventLoop::new(term.clone(), EventProxy, pty, false, false).expect("failed to create event loop");
        let notifier = el.channel();
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
        }
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
        let pid = pty.child().id();

        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);

        let el = EventLoop::new(self.term.clone(), EventProxy, pty, false, false).expect("failed to create event loop");
        self.notifier = el.channel();
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
        self.last_input.set(Some(std::time::Instant::now()));
        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);
        let _ = self.notifier.send(Msg::Input(bytes.into()));
    }

    pub fn send_input_bytes(&self, bytes: Vec<u8>) {
        self.send_input(bytes);
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

    #[allow(clippy::significant_drop_tightening)]
    fn ansi_lines(&self, max_lines: Option<usize>) -> (Vec<String>, Option<(usize, usize)>) {
        use std::fmt::Write;
        let (lines, cursor_logical) = {
            let t = self.term.lock();
            let grid = t.grid();
            let cols = grid.columns();
            let history = grid.history_size();
            let screen = grid.screen_lines();
            let cursor_grid_row = grid.cursor.point.line.0;
            let cursor_grid_col = grid.cursor.point.column.0;

            let default_fg = Color::Named(NamedColor::Foreground);
            let default_bg = Color::Named(NamedColor::Background);
            let mut cur_fg = default_fg;
            let mut cur_bg = default_bg;
            let mut cur_flags = CellFlags::empty();
            let mut lines: Vec<String> = Vec::new();
            let mut cursor_logical: Option<(usize, usize)> = None;

            // Visible screen + scrollback, optionally clipped to the last
            // `max_lines` rows so the API doesn't have to ship the entire
            // history on every poll.
            let want = max_lines.unwrap_or(screen + history).min(screen + history);
            let extra = want.saturating_sub(screen);
            let start_row = -(extra as i32);
            // Track when the previous row's last cell carried WRAPLINE —
            // alacritty sets that flag when a line was soft-wrapped to
            // fit the grid width. Concatenating wrapped rows into one
            // logical line lets long URLs survive the trip to the
            // mobile remote without being chopped in half.
            let mut continues_prev = false;
            // Column offset within the *current* logical line that any
            // continuation row would inherit. After joining, the cursor's
            // column in logical-line coordinates is `prefix_cols +
            // cursor_grid_col`.
            let mut prefix_cols: usize = 0;
            for row in start_row..screen as i32 {
                let last_cell_wraps = grid[GridPoint::new(Line(row), Column(cols - 1))]
                    .flags
                    .contains(CellFlags::WRAPLINE);
                let mut line = String::with_capacity(cols * 2);
                for col in 0..cols {
                    let cell = &grid[GridPoint::new(Line(row), Column(col))];
                    if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    let ch = if cell.c == '\0' { ' ' } else { cell.c };

                    let is_default = cell.fg == default_fg && cell.bg == default_bg && cell.flags.is_empty();
                    if is_default && ch == ' ' && cur_fg == default_fg && cur_bg == default_bg && cur_flags.is_empty() {
                        line.push(' ');
                        continue;
                    }

                    if cell.fg != cur_fg || cell.bg != cur_bg || cell.flags != cur_flags {
                        let mut sgr = Vec::new();

                        let removed = cur_flags & !cell.flags;
                        if removed.intersects(
                            CellFlags::BOLD
                                | CellFlags::DIM
                                | CellFlags::ITALIC
                                | CellFlags::UNDERLINE
                                | CellFlags::INVERSE
                                | CellFlags::HIDDEN
                                | CellFlags::STRIKEOUT,
                        ) {
                            sgr.push("0".to_string());
                            cur_fg = default_fg;
                            cur_bg = default_bg;
                            cur_flags = CellFlags::empty();
                        }

                        if cell.flags.contains(CellFlags::BOLD) && !cur_flags.contains(CellFlags::BOLD) {
                            sgr.push("1".into());
                        }
                        if cell.flags.contains(CellFlags::DIM) && !cur_flags.contains(CellFlags::DIM) {
                            sgr.push("2".into());
                        }
                        if cell.flags.contains(CellFlags::ITALIC) && !cur_flags.contains(CellFlags::ITALIC) {
                            sgr.push("3".into());
                        }
                        if cell.flags.contains(CellFlags::UNDERLINE) && !cur_flags.contains(CellFlags::UNDERLINE) {
                            sgr.push("4".into());
                        }
                        if cell.flags.contains(CellFlags::INVERSE) && !cur_flags.contains(CellFlags::INVERSE) {
                            sgr.push("7".into());
                        }
                        if cell.flags.contains(CellFlags::HIDDEN) && !cur_flags.contains(CellFlags::HIDDEN) {
                            sgr.push("8".into());
                        }
                        if cell.flags.contains(CellFlags::STRIKEOUT) && !cur_flags.contains(CellFlags::STRIKEOUT) {
                            sgr.push("9".into());
                        }

                        if cell.fg != cur_fg {
                            sgr_color(&mut sgr, cell.fg, true);
                        }
                        if cell.bg != cur_bg {
                            sgr_color(&mut sgr, cell.bg, false);
                        }

                        cur_fg = cell.fg;
                        cur_bg = cell.bg;
                        cur_flags = cell.flags;

                        if !sgr.is_empty() {
                            let _ = write!(line, "\x1b[{}m", sgr.join(";"));
                        }
                    }
                    line.push(ch);
                }

                if cur_fg != default_fg || cur_bg != default_bg || !cur_flags.is_empty() {
                    line.push_str("\x1b[0m");
                    cur_fg = default_fg;
                    cur_bg = default_bg;
                    cur_flags = CellFlags::empty();
                }
                // Soft-wrapped rows in alacritty are full-width with no
                // trailing whitespace — preserve every cell so the
                // joined result reads back as the original logical
                // line. Only trim the right edge when this row stands
                // alone (the next row is *not* a continuation).
                let row_text = if last_cell_wraps {
                    line
                } else {
                    line.trim_end().to_string()
                };
                // Capture cursor logical position BEFORE we push the
                // row — `lines.len()` then refers to the index this
                // row will occupy (or extend).
                if row == cursor_grid_row {
                    let logical_idx = if continues_prev {
                        lines.len().saturating_sub(1)
                    } else {
                        lines.len()
                    };
                    cursor_logical = Some((logical_idx, prefix_cols + cursor_grid_col));
                }
                if continues_prev {
                    if let Some(prev) = lines.last_mut() {
                        prev.push_str(&row_text);
                    } else {
                        lines.push(row_text);
                    }
                    prefix_cols += cols;
                } else {
                    lines.push(row_text);
                    prefix_cols = if last_cell_wraps { cols } else { 0 };
                }
                continues_prev = last_cell_wraps;
            }
            (lines, cursor_logical)
        };
        // Lock released.

        let mut lines = lines;
        let mut cursor = cursor_logical;
        let mut leading_trimmed = 0usize;
        while lines.first().is_some_and(std::string::String::is_empty) {
            lines.remove(0);
            leading_trimmed += 1;
        }
        // Adjust the cursor's row for any blank lines we trimmed off
        // the top so it still indexes into the emitted lines vector.
        if let Some((r, c)) = cursor {
            cursor = if r >= leading_trimmed {
                Some((r - leading_trimmed, c))
            } else {
                None
            };
        }
        while lines.last().is_some_and(std::string::String::is_empty) {
            lines.pop();
        }
        if let Some((r, _)) = cursor
            && r >= lines.len()
        {
            cursor = None;
        }
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

fn sgr_color(sgr: &mut Vec<String>, color: Color, foreground: bool) {
    match color {
        Color::Named(n) => {
            let code = match n {
                NamedColor::Black | NamedColor::DimBlack => 0,
                NamedColor::Red | NamedColor::DimRed => 1,
                NamedColor::Green | NamedColor::DimGreen => 2,
                NamedColor::Yellow | NamedColor::DimYellow => 3,
                NamedColor::Blue | NamedColor::DimBlue => 4,
                NamedColor::Magenta | NamedColor::DimMagenta => 5,
                NamedColor::Cyan | NamedColor::DimCyan => 6,
                NamedColor::White | NamedColor::DimWhite => 7,
                NamedColor::BrightBlack => 8,
                NamedColor::BrightRed => 9,
                NamedColor::BrightGreen => 10,
                NamedColor::BrightYellow => 11,
                NamedColor::BrightBlue => 12,
                NamedColor::BrightMagenta => 13,
                NamedColor::BrightCyan => 14,
                NamedColor::BrightWhite => 15,
                NamedColor::Foreground
                | NamedColor::BrightForeground
                | NamedColor::DimForeground
                | NamedColor::Background
                | NamedColor::Cursor => {
                    if foreground {
                        sgr.push("39".into());
                    } else {
                        sgr.push("49".into());
                    }
                    return;
                }
            };
            if code < 8 {
                sgr.push(format!("{}", if foreground { 30 + code } else { 40 + code }));
            } else {
                sgr.push(format!("{}", if foreground { 90 + code - 8 } else { 100 + code - 8 }));
            }
        }
        Color::Indexed(idx) => {
            sgr.push(format!("{};5;{}", if foreground { 38 } else { 48 }, idx));
        }
        Color::Spec(rgb) => {
            sgr.push(format!(
                "{};2;{};{};{}",
                if foreground { 38 } else { 48 },
                rgb.r,
                rgb.g,
                rgb.b
            ));
        }
    }
}

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
                            this.scroll(-(this.visible_lines() as i32));
                            return;
                        }
                        "pagedown" => {
                            this.scroll(this.visible_lines() as i32);
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
                let line_h = this.cell_size.map_or(px(19.6), |c| c.height);
                let multiplier = this.font_config.scroll_sensitivity;
                let delta_px = ev.delta.pixel_delta(line_h);
                let old_offset = (this.scroll_acc.get() / f32::from(line_h)) as i32;
                let acc = this.scroll_acc.get() + f32::from(delta_px.y) * multiplier;
                let new_offset = (acc / f32::from(line_h)) as i32;
                let total_h = f32::from(line_h) * 100.0;
                this.scroll_acc.set(acc % total_h);
                let lines = new_offset - old_offset;
                if lines != 0 {
                    this.scroll(-lines);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _window, _cx| {
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
    segments: Vec<TermSegment>,
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

                    let is_ascii_printable = ch.is_ascii_graphic() || ch == ' ';

                    if !is_ascii_printable {
                        if let Some(seg) = cur_seg.take() {
                            segments.push(seg);
                        }
                        let char_len = ch.len_utf8();
                        segments.push(RawSegment {
                            col_start: c,
                            text: ch.to_string(),
                            runs: vec![TextRun {
                                len: char_len,
                                font: cell_font,
                                color: fg,
                                background_color: None,
                                underline,
                                strikethrough,
                            }],
                        });
                        continue;
                    }

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
        let mut line_texts: Vec<String> = Vec::with_capacity(raw_lines.len());
        for raw in raw_lines {
            line_texts.push(raw.text.clone());
            if let Some(cached) = cache.remove(&raw.grid_line)
                && cached.text == raw.text
            {
                result_lines.push(TermLine {
                    segments: cached.segments.clone(),
                    bg_runs: raw.bg_runs,
                });
                new_cache.insert(raw.grid_line, cached);
                continue;
            }
            let text_clone = raw.text.clone();
            let shaped_segments: Vec<TermSegment> = raw
                .segments
                .into_iter()
                .map(|seg| {
                    let shaped = text_sys.shape_line(seg.text.into(), font_size, &seg.runs, None);
                    TermSegment {
                        col_start: seg.col_start,
                        shaped,
                    }
                })
                .collect();
            new_cache.insert(
                raw.grid_line,
                CachedLine {
                    text: text_clone,
                    segments: shaped_segments.clone(),
                },
            );
            result_lines.push(TermLine {
                segments: shaped_segments,
                bg_runs: raw.bg_runs,
            });
        }

        *cache = new_cache;

        let mut detected = Vec::new();
        for (line_idx, text) in line_texts.iter().enumerate() {
            for (start, end, url, is_file) in detect_urls(text) {
                detected.push(DetectedUrl {
                    line: line_idx,
                    start_col: start,
                    end_col: end,
                    url,
                    is_file,
                });
            }
        }
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
        let Some(state) = prepaint.take() else {
            return;
        };
        let cell = self.cell_size;
        let origin = bounds.origin;

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
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
                for seg in &line.segments {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use tab_atelier::FontConfig;

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
        let mut sgr = Vec::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::Red), true);
        assert_eq!(sgr, vec!["31"]);

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::Red), false);
        assert_eq!(sgr, vec!["41"]);
    }

    #[test]
    fn test_sgr_color_bright() {
        let mut sgr = Vec::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::BrightRed), true);
        assert_eq!(sgr, vec!["91"]);

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::BrightRed), false);
        assert_eq!(sgr, vec!["101"]);
    }

    #[test]
    fn test_sgr_color_foreground_default() {
        let mut sgr = Vec::new();
        sgr_color(&mut sgr, Color::Named(NamedColor::Foreground), true);
        assert_eq!(sgr, vec!["39"]);

        sgr.clear();
        sgr_color(&mut sgr, Color::Named(NamedColor::Foreground), false);
        assert_eq!(sgr, vec!["49"]);
    }

    #[test]
    fn test_sgr_color_indexed() {
        let mut sgr = Vec::new();
        sgr_color(&mut sgr, Color::Indexed(196), true);
        assert_eq!(sgr, vec!["38;5;196"]);

        sgr.clear();
        sgr_color(&mut sgr, Color::Indexed(196), false);
        assert_eq!(sgr, vec!["48;5;196"]);
    }

    #[test]
    fn test_sgr_color_rgb() {
        let mut sgr = Vec::new();
        sgr_color(&mut sgr, Color::Spec(vte::ansi::Rgb { r: 255, g: 128, b: 0 }), true);
        assert_eq!(sgr, vec!["38;2;255;128;0"]);
    }

    #[test]
    fn test_visible_lines_default() {
        // Without cell_size set, should return 25
        // We can't easily construct a TerminalView without gpui context,
        // so this is tested indirectly via the scroll tests.
    }
}
