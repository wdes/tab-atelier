// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PTY scrollback → ANSI-encoded text serialization.
//!
//! The HTTP API (`GET /tabs/{idx}/output`) wants a recent slice of
//! the alacritty grid rendered out as a normal terminal byte stream —
//! SGR-escaped colours and attributes intact — so a downstream client
//! can pipe it straight into another terminal and see what the user
//! sees. Both the GUI's `TerminalView` and the headless `HeadlessTab`
//! keep an `Arc<FairMutex<Term<E>>>` and produce these strings the
//! same way; this module hosts that one implementation, generic over
//! the `EventListener` `E` so the two callers don't drift.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as GridPoint};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use std::fmt::Write;
use vte::ansi::{Color, NamedColor};

/// Grid-derived snapshot fields, memoised between API-snapshot
/// refreshes and keyed by the PTY ring's monotonic `total_len`.
///
/// Both the GUI (`app.rs`) and headless (`headless.rs`) persist paths
/// scanned the alacritty grid (`ansi_text_with_cursor(200)` +
/// 2000-row `raw_screen_text`) for every tab on every tick. Because
/// every byte that can change the grid flows through the PTY ring
/// first, a `ring_len` equal to the cached one means the grid is
/// byte-for-byte unchanged and the prior scan can be reused, so idle
/// tabs stop paying for the full-grid walk each tick.
/// The two grid dumps are `Arc<str>` rather than `String`: the cache is
/// cloned into the API snapshot for EVERY tab on every refresh tick
/// (2 s GUI persist, ~96 ms headless), and `raw_output` alone can be
/// hundreds of KB with a viewer attached — as owned `String`s that was
/// a multi-MB memcpy + allocator churn per tick even when fully idle.
/// A clone is now two refcount bumps.
#[derive(Clone)]
pub struct GridSnapshotCache {
    pub ring_len: u64,
    pub output: std::sync::Arc<str>,
    pub cursor: Option<(usize, usize)>,
    pub raw_output: std::sync::Arc<str>,
    pub raw_cursor: Option<(usize, usize)>,
    pub cols: u16,
    pub rows: u16,
    /// CRC32 of `output` / `raw_output`, computed once when the dump is
    /// (re)built. `GET /output` needs the payload's total CRC on every
    /// poll (the `X-Output-Crc` header + the `?since`/`crc` delta
    /// handshake); recomputing it per request walked up to hundreds of
    /// KB at the poll rate for a value that only changes when the grid
    /// does.
    pub output_crc: u32,
    pub raw_output_crc: u32,
}

impl GridSnapshotCache {
    /// Build the cache from freshly-scanned dumps, stamping their CRCs.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ring_len: u64,
        output: String,
        cursor: Option<(usize, usize)>,
        raw_output: String,
        raw_cursor: Option<(usize, usize)>,
        cols: u16,
        rows: u16,
    ) -> Self {
        let output_crc = crate::crc32(output.as_bytes());
        let raw_output_crc = crate::crc32(raw_output.as_bytes());
        Self {
            ring_len,
            output: output.into(),
            cursor,
            raw_output: raw_output.into(),
            raw_cursor,
            cols,
            rows,
            output_crc,
            raw_output_crc,
        }
    }

    /// This cache with the 2000-row `raw_output` dump released. Used when
    /// the last web viewer detaches from a tab that then goes quiet: the
    /// rebuild-on-ring-advance path would drop the dump on the next byte
    /// of output, but a silent tab would otherwise pin megabytes of
    /// scrollback text nobody can read anymore. Everything else is shared
    /// (`Arc` bumps), so no grid rescan happens.
    #[must_use]
    pub fn without_raw(&self) -> Self {
        Self {
            ring_len: self.ring_len,
            output: std::sync::Arc::clone(&self.output),
            cursor: self.cursor,
            raw_output: std::sync::Arc::from(""),
            raw_cursor: None,
            cols: self.cols,
            rows: self.rows,
            output_crc: self.output_crc,
            raw_output_crc: crate::crc32(b""),
        }
    }
}

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

    // One SGR scratch buffer for the whole walk — a per-cell-coloured
    // dump used to allocate a fresh String per attribute change.
    let mut sgr = String::new();
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
                sgr.clear();
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
            // Trim in place — `trim_end().to_string()` re-allocated a
            // second full String per non-wrapped row.
            let mut line = line;
            line.truncate(line.trim_end().len());
            line
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
    // Size for the rows we actually emit (`want`), not just the screen —
    // the 2000-row dump was under-sized ~40x and paid doubling reallocs.
    let mut out = String::with_capacity(cols * want * 2);

    // Row + SGR scratch buffers hoisted out of the loops: only their
    // trimmed slice ever reaches `out`, so the whole dump now runs with
    // zero per-row allocations.
    let mut line = String::with_capacity(cols * 2);
    let mut sgr = String::new();
    for row in start_row..screen as i32 {
        line.clear();
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
                sgr.clear();
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
                // Scope the closure in a block so its borrow of `sgr`
                // is released before sgr_color() takes another &mut.
                {
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
        // Trim trailing default-bg space so a line of 240 default
        // spaces doesn't bloat the transfer. xterm.js advances cursor
        // to col 0 on \n, so the missing trailing spaces don't change
        // the layout when the next row is written.
        // `trim_end_matches` returns a borrowed slice — push it straight into
        // `out` instead of allocating a fresh `String` per row.
        out.push_str(line.trim_end_matches(' '));
        out.push('\n');
    }
    drop(t);
    (out, cursor_dump)
}

/// `term_to_ansi_lines` then trim leading / trailing empty rows and
/// re-anchor the cursor, finally joining with `\n`. Matches the
/// shape the API (`GET /tabs/{idx}/output`) expects.
pub fn term_to_ansi_text_with_cursor<E: EventListener>(
    term: &FairMutex<Term<E>>,
    max_lines: Option<usize>,
) -> (String, Option<(usize, usize)>) {
    let (mut lines, mut cursor) = term_to_ansi_lines(term, max_lines);
    // Count then drain — `remove(0)` per empty row memmoves the whole
    // Vec (O(k x n) on a dump of up to ~2000 rows).
    let leading_trimmed = lines.iter().take_while(|l| l.is_empty()).count();
    lines.drain(..leading_trimmed);
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

#[cfg(test)]
mod walk_tests {
    use super::*;

    struct Nop;
    impl EventListener for Nop {}

    /// A live 20×5 terminal fed through the real vte parser.
    fn term_with(bytes: &[u8]) -> FairMutex<Term<Nop>> {
        let term = Term::new(
            alacritty_terminal::term::Config::default(),
            &TermDims {
                columns: 20,
                screen_lines: 5,
            },
            Nop,
        );
        let term = FairMutex::new(term);
        let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
        parser.advance(&mut *term.lock(), bytes);
        term
    }

    #[test]
    fn lines_walk_trims_joins_and_emits_sgr() {
        // Red word, trailing spaces (must trim), and a soft-wrapped long
        // line (25 chars in a 20-col grid ⇒ WRAPLINE join).
        let term = term_with(b"\x1b[31mred\x1b[0m   \r\nabcdefghijklmnopqrstuvwxy");
        let (text, _cursor) = term_to_ansi_text_with_cursor(&term, None);
        let lines: Vec<&str> = text.split('\n').collect();
        assert!(lines[0].contains("\x1b[31m"), "SGR preserved: {lines:?}");
        // A fg-only change back to default resets via SGR 39, and the
        // trailing default spaces are trimmed away.
        assert!(lines[0].ends_with("\x1b[39m"), "fg reset closes the row: {lines:?}");
        assert_eq!(
            crate::strip_ansi(lines[1]),
            "abcdefghijklmnopqrstuvwxy",
            "wrapped rows joined into one logical line: {lines:?}"
        );
        assert_eq!(lines.len(), 2, "trailing empty rows trimmed");
    }

    #[test]
    fn rows_walk_is_row_per_line_with_no_join() {
        let term = term_with(b"abcdefghijklmnopqrstuvwxy");
        let (text, cursor) = term_to_ansi_rows(&term, Some(2000));
        let rows: Vec<&str> = text.split('\n').collect();
        // (The soft-wrap flag on the row's last cell emits a trailing
        // SGR reset — strip escapes and compare the text content.)
        assert_eq!(
            crate::strip_ansi(rows[0]),
            "abcdefghijklmnopqrst",
            "first grid row verbatim"
        );
        assert_eq!(crate::strip_ansi(rows[1]), "uvwxy", "wrap NOT joined in row mode");
        assert!(cursor.is_some(), "cursor on the visible screen");
    }

    #[test]
    fn leading_empty_rows_are_drained() {
        // Cursor parked mid-screen: rows above stay empty and must be
        // dropped from the joined dump.
        let term = term_with(b"\x1b[3;1Hdown here");
        let (text, _cursor) = term_to_ansi_text_with_cursor(&term, None);
        assert_eq!(text, "down here");
    }
}

#[cfg(test)]
mod cache_tests {
    use super::GridSnapshotCache;

    #[test]
    fn new_stamps_crcs_of_both_dumps() {
        let g = GridSnapshotCache::new(
            42,
            "hello".to_string(),
            Some((1, 2)),
            "raw rows".to_string(),
            None,
            80,
            24,
        );
        assert_eq!(g.ring_len, 42);
        assert_eq!(&*g.output, "hello");
        assert_eq!(&*g.raw_output, "raw rows");
        assert_eq!(g.output_crc, crate::crc32(b"hello"));
        assert_eq!(g.raw_output_crc, crate::crc32(b"raw rows"));
        assert_eq!((g.cols, g.rows), (80, 24));
        assert_eq!(g.cursor, Some((1, 2)));
        assert_eq!(g.raw_cursor, None);
        // Clones share the dumps (refcount bump, not memcpy).
        let c = g.clone();
        assert!(std::sync::Arc::ptr_eq(&g.output, &c.output));
        assert!(std::sync::Arc::ptr_eq(&g.raw_output, &c.raw_output));
    }

    #[test]
    fn without_raw_sheds_the_dump_and_keeps_the_rest() {
        let g = GridSnapshotCache::new(
            42,
            "hello".to_string(),
            Some((1, 2)),
            "raw rows".to_string(),
            Some((3, 4)),
            80,
            24,
        );
        let d = g.without_raw();
        assert!(d.raw_output.is_empty());
        assert_eq!(d.raw_cursor, None);
        assert_eq!(d.raw_output_crc, crate::crc32(b""));
        // Everything else survives — shared, not copied.
        assert!(std::sync::Arc::ptr_eq(&g.output, &d.output));
        assert_eq!(d.ring_len, 42);
        assert_eq!(d.output_crc, g.output_crc);
        assert_eq!((d.cols, d.rows, d.cursor), (80, 24, Some((1, 2))));
    }
}
