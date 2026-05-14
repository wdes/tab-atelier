// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use gpui::*;
use crate::terminal_utils::{
    color_to_hsla, hsla_eq, is_default_bg, is_default_fg, keystroke_to_bytes, DEFAULT_BG,
    DEFAULT_FG,
};
use swoop::FontConfig;

const INITIAL_COLS: usize = 80;
const INITIAL_LINES: usize = 24;
const SCROLLBAR_WIDTH: f32 = 8.0;

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

pub struct TerminalView {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: EventLoopSender,
    focus: FocusHandle,
    cell_size: Option<Size<Pixels>>,
    last_size: Rc<Cell<Option<(usize, usize)>>>,
    content_origin: Rc<Cell<gpui::Point<Pixels>>>,
    bounds_size: Rc<Cell<Size<Pixels>>>,
    pid: u32,
    exited: Rc<Cell<bool>>,
    scrollbar_dragging: Rc<Cell<bool>>,
    font_config: FontConfig,
}

impl TerminalView {
    pub fn new(
        cwd: Option<&Path>,
        font_config: FontConfig,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ws = WindowSize {
            num_lines: INITIAL_LINES as u16,
            num_cols: INITIAL_COLS as u16,
            cell_width: 9,
            cell_height: 18,
        };
        let mut opts = tty::Options::default();
        opts.working_directory = cwd.map(|p| p.to_path_buf());
        let pty = tty::new(&opts, ws, 0).expect("failed to create pty");
        let pid = pty.child().id();
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &TermDims { columns: INITIAL_COLS, screen_lines: INITIAL_LINES },
            EventProxy,
        );
        let term = Arc::new(FairMutex::new(term));
        let el = EventLoop::new(term.clone(), EventProxy, pty, false, false)
            .expect("failed to create event loop");
        let notifier = el.channel();
        el.spawn();

        let focus = cx.focus_handle();

        cx.spawn(async |this: WeakEntity<TerminalView>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let Ok(()) = this.update(cx, |_, cx: &mut Context<TerminalView>| cx.notify()) else {
                    break;
                };
            }
        })
        .detach();

        let exited = Rc::new(Cell::new(false));
        let exited_clone = exited.clone();
        let pid_for_check = pid;
        cx.spawn(async move |this: WeakEntity<TerminalView>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if !Path::new(&format!("/proc/{pid_for_check}")).exists() {
                    exited_clone.set(true);
                    let _ = this.update(cx, |_, cx: &mut Context<TerminalView>| cx.notify());
                    break;
                }
            }
        })
        .detach();

        Self {
            term, notifier, focus, cell_size: None,
            last_size: Rc::new(Cell::new(None)),
            content_origin: Rc::new(Cell::new(point(px(0.0), px(0.0)))),
            bounds_size: Rc::new(Cell::new(size(px(0.0), px(0.0)))),
            pid,
            exited,
            scrollbar_dragging: Rc::new(Cell::new(false)),
            font_config,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn has_exited(&self) -> bool {
        self.exited.get()
    }

    pub fn respawn(&mut self, cwd: Option<&Path>, cx: &mut Context<Self>) {
        let _ = self.notifier.send(Msg::Shutdown);

        let (cols, lines) = self.last_size.get().unwrap_or((INITIAL_COLS, INITIAL_LINES));
        let cell = self.cell_size.unwrap_or(Size { width: px(8.4), height: px(19.6) });

        let ws = WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: f32::from(cell.width) as u16,
            cell_height: f32::from(cell.height) as u16,
        };

        let mut opts = tty::Options::default();
        opts.working_directory = cwd.map(|p| p.to_path_buf());
        let pty = tty::new(&opts, ws, 0).expect("failed to create pty");
        let pid = pty.child().id();

        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);

        let el = EventLoop::new(self.term.clone(), EventProxy, pty, false, false)
            .expect("failed to create event loop");
        self.notifier = el.channel();
        el.spawn();

        self.pid = pid;
        self.exited.set(false);

        let exited = self.exited.clone();
        let pid_for_check = pid;
        cx.spawn(async move |this: WeakEntity<TerminalView>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if !Path::new(&format!("/proc/{pid_for_check}")).exists() {
                    exited.set(true);
                    let _ = this.update(cx, |_, cx: &mut Context<TerminalView>| cx.notify());
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
        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);
        let _ = self.notifier.send(Msg::Input(bytes.into()));
    }

    pub fn send_clipboard(&self, text: String) {
        self.term.lock().grid_mut().scroll_display(Scroll::Bottom);
        let bracketed = format!("\x1b[200~{text}\x1b[201~");
        let _ = self.notifier.send(Msg::Input(bracketed.into_bytes().into()));
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

    pub fn copy_selection(&self) -> Option<String> {
        let t = self.term.lock();
        t.selection_to_string()
    }

    pub fn copy_all_history(&self) -> String {
        let t = self.term.lock();
        let grid = t.grid();
        let cols = grid.columns();
        let history = grid.history_size();
        let screen = grid.screen_lines();
        let mut lines = Vec::new();

        for row in (-(history as i32))..screen as i32 {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &grid[GridPoint::new(Line(row), Column(col))];
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                line.push(ch);
            }
            lines.push(line.trim_end().to_string());
        }

        // Trim leading and trailing empty lines
        while lines.first().is_some_and(|l| l.is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn pixel_to_grid(&self, pos: gpui::Point<Pixels>, bounds_origin: gpui::Point<Pixels>) -> (GridPoint, Side) {
        let cell = self.cell_size.unwrap_or(Size { width: px(8.4), height: px(19.6) });
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
                    font: f.clone(),
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
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _window, _cx| {
                if let Some(bytes) = keystroke_to_bytes(&ev.keystroke) {
                    this.clear_selection();
                    this.send_input(bytes);
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _window, cx| {
                let cell_h = f32::from(this.cell_size.map_or(px(19.6), |c| c.height));
                let lines = match ev.delta {
                    ScrollDelta::Lines(pt) => -pt.y as i32,
                    ScrollDelta::Pixels(pt) => -(f32::from(pt.y) / cell_h) as i32,
                };
                if lines != 0 {
                    this.scroll(lines);
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _window, _cx| {
                let origin = this.content_origin.get();
                let bounds = this.bounds_size.get();
                let scrollbar_left = origin.x + bounds.width - px(SCROLLBAR_WIDTH);
                if ev.position.x >= scrollbar_left {
                    this.scrollbar_dragging.set(true);
                    let y_frac = f32::from(ev.position.y - origin.y) / f32::from(bounds.height);
                    this.scroll_to_fraction(y_frac.clamp(0.0, 1.0));
                } else {
                    let (gp, side) = this.pixel_to_grid(ev.position, origin);
                    this.start_selection(gp, side);
                }
            }))
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _window, _cx| {
                if this.scrollbar_dragging.get() {
                    let origin = this.content_origin.get();
                    let bounds = this.bounds_size.get();
                    let y_frac = f32::from(ev.position.y - origin.y) / f32::from(bounds.height);
                    this.scroll_to_fraction(y_frac.clamp(0.0, 1.0));
                } else if ev.pressed_button == Some(MouseButton::Left) {
                    let origin = this.content_origin.get();
                    let (gp, side) = this.pixel_to_grid(ev.position, origin);
                    this.update_selection(gp, side);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, _ev: &MouseUpEvent, _window, _cx| {
                this.scrollbar_dragging.set(false);
            }))
            .size_full()
            .child(TerminalElement {
                term,
                notifier: self.notifier.clone(),
                cell_size,
                last_size: self.last_size.clone(),
                content_origin: self.content_origin.clone(),
                bounds_size: self.bounds_size.clone(),
                font_config: self.font_config.clone(),
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
    font_config: FontConfig,
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

struct TermLine {
    shaped: ShapedLine,
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
        _window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = _window.request_layout(Style {
            size: Size {
                width: relative(1.0).into(),
                height: relative(1.0).into(),
            },
            ..Default::default()
        }, [], _cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let cell = self.cell_size;
        let cols = ((bounds.size.width / cell.width) as usize).max(2);
        let lines = ((bounds.size.height / cell.height) as usize).max(1);

        if self.last_size.get() != Some((cols, lines)) {
            self.last_size.set(Some((cols, lines)));
            {
                let mut t = self.term.lock();
                t.resize(TermDims { columns: cols, screen_lines: lines });
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
        let fg_default: Hsla = rgb(DEFAULT_FG).into();

        // Phase 1: read cell data under the lock — no shaping here.
        struct RawLine {
            text: String,
            runs: Vec<TextRun>,
            bg_runs: Vec<BgRun>,
        }

        let (raw_lines, cursor, selection, visible_cols, display_offset_val, history_size) = {
            let term = self.term.lock();
            let grid = term.grid();
            let cursor_point = grid.cursor.point;
            let display_offset = grid.display_offset() as i32;
            let visible_lines = grid.screen_lines().min(lines);
            let visible_cols = grid.columns().min(cols);

            let mut raw_lines = Vec::with_capacity(visible_lines);

            for l in 0..visible_lines {
                let mut text = String::with_capacity(visible_cols);
                let mut runs: Vec<TextRun> = Vec::new();
                let mut bg_runs: Vec<BgRun> = Vec::new();

                for c in 0..visible_cols {
                    let cell_data = &grid[GridPoint::new(Line(l as i32 - display_offset), Column(c))];
                    if cell_data.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        text.push(' ');
                        let char_len = 1;
                        let can_merge = !runs.is_empty();
                        if can_merge {
                            runs.last_mut().unwrap().len += char_len;
                        } else {
                            let fg = if is_default_fg(cell_data.fg) {
                                fg_default
                            } else {
                                color_to_hsla(cell_data.fg)
                            };
                            let mut spacer_font = font(mono_font.family.clone());
                            spacer_font.weight = mono_font.weight;
                            runs.push(TextRun {
                                len: char_len,
                                font: spacer_font,
                                color: fg,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            });
                        }
                        continue;
                    }
                    let ch = if cell_data.c == '\0' { ' ' } else { cell_data.c };
                    text.push(ch);

                    let mut fg = if is_default_fg(cell_data.fg) {
                        fg_default
                    } else {
                        color_to_hsla(cell_data.fg)
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
                            rgb(DEFAULT_BG).into()
                        } else {
                            color_to_hsla(cell_data.bg)
                        };
                        let old_fg = fg;
                        fg = bg_c;
                        if let Some(last) = bg_runs.last_mut() {
                            if last.col + last.len == c && hsla_eq(last.color, old_fg) {
                                last.len += 1;
                            } else {
                                bg_runs.push(BgRun { col: c, len: 1, color: old_fg });
                            }
                        } else {
                            bg_runs.push(BgRun { col: c, len: 1, color: old_fg });
                        }
                    } else if !is_default_bg(cell_data.bg) {
                        let bg_c = color_to_hsla(cell_data.bg);
                        if let Some(last) = bg_runs.last_mut() {
                            if last.col + last.len == c && hsla_eq(last.color, bg_c) {
                                last.len += 1;
                            } else {
                                bg_runs.push(BgRun { col: c, len: 1, color: bg_c });
                            }
                        } else {
                            bg_runs.push(BgRun { col: c, len: 1, color: bg_c });
                        }
                    }

                    let mut cell_font = font(mono_font.family.clone());
                    cell_font.weight = font_weight;
                    cell_font.style = font_style;

                    let char_len = ch.len_utf8();
                    let can_merge = runs.last().map_or(false, |last: &TextRun| {
                        last.color == fg
                            && last.font == cell_font
                            && last.underline == underline
                            && last.strikethrough == strikethrough
                    });
                    if can_merge {
                        runs.last_mut().unwrap().len += char_len;
                    } else {
                        runs.push(TextRun {
                            len: char_len,
                            font: cell_font,
                            color: fg,
                            background_color: None,
                            underline,
                            strikethrough,
                        });
                    }
                }

                raw_lines.push(RawLine { text, runs, bg_runs });
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

            (raw_lines, cursor, selection, visible_cols, display_offset as usize, history_size)
        };
        // Lock released — event loop can proceed while we shape text.

        // Phase 2: shape lines without holding the lock.
        let text_sys = window.text_system();
        let mut result_lines = Vec::with_capacity(raw_lines.len());
        for raw in raw_lines {
            let shaped = text_sys.shape_line(
                raw.text.into(),
                font_size,
                &raw.runs,
                Some(cell.width),
            );
            result_lines.push(TermLine { shaped, bg_runs: raw.bg_runs });
        }

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
        _: &mut Self::RequestLayoutState,
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

            // Paint selection.
            if let Some(ref sel) = state.selection {
                let sel_color = Hsla::from(Rgba { r: 0.2, g: 0.4, b: 0.7, a: 0.5 });
                let start = sel.start;
                let end = sel.end;
                for row in start.line.0..=end.line.0 {
                    if row < 0 || row as usize >= state.lines.len() {
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
                        origin.y + cell.height * row as f32,
                    );
                    let sz = size(cell.width * (col_end - col_start) as f32, cell.height);
                    window.paint_quad(fill(Bounds::new(pos, sz), sel_color));
                }
            }

            // Paint text.
            for (line_idx, line) in state.lines.iter().enumerate() {
                let pos = point(
                    origin.x,
                    origin.y + cell.height * line_idx as f32,
                );
                let _ = line.shaped.paint(pos, cell.height, window, cx);
            }

            // Paint cursor.
            if let Some((row, col)) = state.cursor {
                let pos = point(
                    origin.x + cell.width * col as f32,
                    origin.y + cell.height * row as f32,
                );
                let cursor_size = size(cell.width, cell.height);
                window.paint_quad(fill(
                    Bounds::new(pos, cursor_size),
                    Hsla::from(Rgba { r: 0.86, g: 0.86, b: 0.86, a: 0.7 }),
                ));
            }

            // Paint scrollbar.
            if state.history_size > 0 {
                let sb_width = px(SCROLLBAR_WIDTH);
                let track_left = origin.x + bounds.size.width - sb_width;
                let track_height = bounds.size.height;

                let track_bounds = Bounds::new(
                    point(track_left, origin.y),
                    size(sb_width, track_height),
                );
                window.paint_quad(fill(track_bounds, Hsla::from(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.05 })));

                let total = (state.history_size + state.lines.len()) as f32;
                let visible_frac = state.lines.len() as f32 / total;
                let thumb_h = (visible_frac * f32::from(track_height)).max(20.0);
                let thumb_h = px(thumb_h);
                let max_offset = state.history_size as f32;
                let scroll_frac = 1.0 - (state.display_offset as f32 / max_offset);
                let available = track_height - thumb_h;
                let thumb_top = origin.y + available * scroll_frac;

                let thumb_bounds = Bounds::new(
                    point(track_left, thumb_top),
                    size(sb_width, thumb_h),
                );
                let thumb_color = if state.display_offset > 0 {
                    Hsla::from(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.4 })
                } else {
                    Hsla::from(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.2 })
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

