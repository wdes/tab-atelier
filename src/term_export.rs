// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PTY scrollback → ANSI-encoded text serialization.
//!
//! The HTTP API (`GET /tabs/{idx}/output`) and the happier-bridge
//! publisher both want a recent slice of the alacritty grid rendered
//! out as a normal terminal byte stream — SGR-escaped colours and
//! attributes intact — so a downstream client can pipe it straight
//! into another terminal and see what the user sees. Both the GUI's
//! `TerminalView` and the headless `HeadlessTab` keep an
//! `Arc<FairMutex<Term<E>>>` and produce these strings the same way;
//! this module hosts that one implementation, generic over the
//! `EventListener` `E` so the two callers don't drift.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as GridPoint};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use std::fmt::Write;
use vte::ansi::{Color, NamedColor};

/// Minimal `Dimensions` impl for constructing a `Term`.
///
/// Both the GUI and headless tab spawners use the same
/// `INITIAL_COLS` / `INITIAL_LINES` seed when first creating a tab;
/// the actual grid resizes from there.
pub struct TermDims {
    pub columns: usize,
    pub screen_lines: usize,
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

/// Render the visible screen + up to `max_lines` of scrollback as
/// individual logical lines (wrapped rows already joined), with SGR
/// escapes preserved. Returns the lines and the cursor's position in
/// logical-line coordinates (None when the cursor is outside the
/// emitted window).
#[allow(clippy::significant_drop_tightening)]
#[allow(clippy::too_many_lines)]
pub fn term_to_ansi_lines<E: EventListener>(
    term: &FairMutex<Term<E>>,
    max_lines: Option<usize>,
) -> (Vec<String>, Option<(usize, usize)>) {
    let t = term.lock();
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

    let want = max_lines.unwrap_or(screen + history).min(screen + history);
    let extra = want.saturating_sub(screen);
    let start_row = -(extra as i32);
    // Track when the previous row's last cell carried WRAPLINE — soft-wrapped
    // rows get glued back together into one logical line so long URLs etc.
    // survive intact.
    let mut continues_prev = false;
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
                let mut sgr = String::new();
                let push_code = |buf: &mut String, code: &str| {
                    if !buf.is_empty() {
                        buf.push(';');
                    }
                    buf.push_str(code);
                };

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
                    push_code(&mut sgr, "0");
                    cur_fg = default_fg;
                    cur_bg = default_bg;
                    cur_flags = CellFlags::empty();
                }

                if cell.flags.contains(CellFlags::BOLD) && !cur_flags.contains(CellFlags::BOLD) {
                    push_code(&mut sgr, "1");
                }
                if cell.flags.contains(CellFlags::DIM) && !cur_flags.contains(CellFlags::DIM) {
                    push_code(&mut sgr, "2");
                }
                if cell.flags.contains(CellFlags::ITALIC) && !cur_flags.contains(CellFlags::ITALIC) {
                    push_code(&mut sgr, "3");
                }
                if cell.flags.contains(CellFlags::UNDERLINE) && !cur_flags.contains(CellFlags::UNDERLINE) {
                    push_code(&mut sgr, "4");
                }
                if cell.flags.contains(CellFlags::INVERSE) && !cur_flags.contains(CellFlags::INVERSE) {
                    push_code(&mut sgr, "7");
                }
                if cell.flags.contains(CellFlags::HIDDEN) && !cur_flags.contains(CellFlags::HIDDEN) {
                    push_code(&mut sgr, "8");
                }
                if cell.flags.contains(CellFlags::STRIKEOUT) && !cur_flags.contains(CellFlags::STRIKEOUT) {
                    push_code(&mut sgr, "9");
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
                    let _ = write!(line, "\x1b[{sgr}m");
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
        let row_text = if last_cell_wraps {
            line
        } else {
            line.trim_end().to_string()
        };
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
}

/// Row-by-row dump for the xterm.js viewer — does NOT join WRAPLINE
/// rows back into logical lines. Each emitted row corresponds 1:1
/// with a server-grid row, so xterm.js (resized to the same cols)
/// reproduces the visual layout cell-for-cell instead of relying on
/// auto-wrap to land at the same column the server's auto-wrap did.
/// `\n` between rows; no trim.
pub fn term_to_ansi_rows<E: EventListener>(
    term: &FairMutex<Term<E>>,
    max_lines: Option<usize>,
) -> (String, Option<(usize, usize)>) {
    // term_to_ansi_lines glues wrap-continued rows into one entry.
    // We need the raw row-by-row dump, so re-walk the grid directly.
    let t = term.lock();
    let grid = t.grid();
    let cols = grid.columns();
    let history = grid.history_size();
    let screen = grid.screen_lines();
    let cursor_grid_row = grid.cursor.point.line.0;
    let cursor_grid_col = grid.cursor.point.column.0;
    let want = max_lines.unwrap_or(screen + history).min(screen + history);
    let extra = want.saturating_sub(screen);
    let start_row = -(extra as i32);
    // Cursor row in the emitted dump (0-indexed from first emitted
    // row). None when the cursor is in scrollback above the window
    // we're shipping — e.g. user scrolled the GUI up past max_lines.
    let cursor_dump = if cursor_grid_row >= start_row && cursor_grid_row < screen as i32 {
        Some(((cursor_grid_row - start_row) as usize, cursor_grid_col))
    } else {
        None
    };

    let default_fg = Color::Named(NamedColor::Foreground);
    let default_bg = Color::Named(NamedColor::Background);
    let mut cur_fg = default_fg;
    let mut cur_bg = default_bg;
    let mut cur_flags = CellFlags::empty();
    let mut out = String::with_capacity(cols * screen * 2);

    for row in start_row..screen as i32 {
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
                let mut sgr = String::new();
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
                    sgr.push('0');
                    cur_fg = default_fg;
                    cur_bg = default_bg;
                    cur_flags = CellFlags::empty();
                }
                let mut push = |code: &str| {
                    if !sgr.is_empty() {
                        sgr.push(';');
                    }
                    sgr.push_str(code);
                };
                if cell.flags.contains(CellFlags::BOLD) && !cur_flags.contains(CellFlags::BOLD) {
                    push("1");
                }
                if cell.flags.contains(CellFlags::DIM) && !cur_flags.contains(CellFlags::DIM) {
                    push("2");
                }
                if cell.flags.contains(CellFlags::ITALIC) && !cur_flags.contains(CellFlags::ITALIC) {
                    push("3");
                }
                if cell.flags.contains(CellFlags::UNDERLINE) && !cur_flags.contains(CellFlags::UNDERLINE) {
                    push("4");
                }
                if cell.flags.contains(CellFlags::INVERSE) && !cur_flags.contains(CellFlags::INVERSE) {
                    push("7");
                }
                if cell.flags.contains(CellFlags::HIDDEN) && !cur_flags.contains(CellFlags::HIDDEN) {
                    push("8");
                }
                if cell.flags.contains(CellFlags::STRIKEOUT) && !cur_flags.contains(CellFlags::STRIKEOUT) {
                    push("9");
                }
                drop(push);
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
                    let _ = write!(line, "\x1b[{sgr}m");
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
        // Trim trailing default-bg space so a line of 240 default
        // spaces doesn't bloat the transfer. xterm.js advances cursor
        // to col 0 on \n, so the missing trailing spaces don't change
        // the layout when the next row is written.
        let trimmed = line.trim_end_matches(' ').to_string();
        out.push_str(&trimmed);
        out.push('\n');
    }
    drop(t);
    (out, cursor_dump)
}

/// `term_to_ansi_lines` then trim leading / trailing empty rows and
/// re-anchor the cursor, finally joining with `\n`. Matches the
/// shape the API (`GET /tabs/{idx}/output`) and the happier-bridge
/// publisher expect.
pub fn term_to_ansi_text_with_cursor<E: EventListener>(
    term: &FairMutex<Term<E>>,
    max_lines: Option<usize>,
) -> (String, Option<(usize, usize)>) {
    let (mut lines, mut cursor) = term_to_ansi_lines(term, max_lines);
    let mut leading_trimmed = 0_usize;
    while lines.first().is_some_and(std::string::String::is_empty) {
        lines.remove(0);
        leading_trimmed += 1;
    }
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
    (lines.join("\n"), cursor)
}

/// Append the numeric payload for an SGR colour escape — `\e[<code>m`'s
/// `<code>` part — into the buffer, separating from the previous
/// parameter with `;`. Writes directly into the buffer instead of
/// allocating per-code Strings; this is on the per-paint hot path
/// and a typical coloured frame calls it hundreds of times.
pub fn sgr_color(sgr: &mut String, color: Color, foreground: bool) {
    if !sgr.is_empty() {
        sgr.push(';');
    }
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
                    sgr.push_str(if foreground { "39" } else { "49" });
                    return;
                }
            };
            let n = if code < 8 {
                if foreground { 30 + code } else { 40 + code }
            } else if foreground {
                90 + code - 8
            } else {
                100 + code - 8
            };
            let _ = write!(sgr, "{n}");
        }
        Color::Indexed(idx) => {
            let _ = write!(sgr, "{};5;{}", if foreground { 38 } else { 48 }, idx);
        }
        Color::Spec(rgb) => {
            let _ = write!(
                sgr,
                "{};2;{};{};{}",
                if foreground { 38 } else { 48 },
                rgb.r,
                rgb.g,
                rgb.b
            );
        }
    }
}
