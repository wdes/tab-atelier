// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "gui")]

//! Geometric box-drawing.
//!
//! Terminal box-drawing glyphs (U+2500–U+257F) coming from the font —
//! or worse, a fallback font when the primary monospace lacks them —
//! are usually drawn NARROWER than the character cell. `shape_line`'s
//! `force_width` clamps the glyph *advance* to one cell but can't widen
//! the ink, so a run of `─` renders dashed (gaps between cells) and the
//! vertical rules of a table drift out of alignment.
//!
//! Like Ghostty / Kitty / Alacritty, we draw the common subset
//! ourselves: each glyph is a set of filled bars from the cell centre
//! to its edges, so light/heavy lines connect edge-to-edge regardless
//! of the active font. Double-line and diagonal glyphs aren't handled
//! and fall back to the font glyph (they're rare in TUIs).

/// Stroke weight from the cell centre toward one edge.
/// `0` = none, `1` = light, `2` = heavy.
type Weight = u8;

/// Per-direction stroke weights of a box-drawing glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BoxParts {
    pub up: Weight,
    pub right: Weight,
    pub down: Weight,
    pub left: Weight,
}

impl BoxParts {
    const fn new(up: Weight, right: Weight, down: Weight, left: Weight) -> Self {
        Self { up, right, down, left }
    }
    /// Horizontal line of weight `w` (left+right).
    const fn h(w: Weight) -> Self {
        Self::new(0, w, 0, w)
    }
    /// Vertical line of weight `w` (up+down).
    const fn v(w: Weight) -> Self {
        Self::new(w, 0, w, 0)
    }
}

/// Cell-relative rectangle (origin = cell top-left), in pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Map a box-drawing codepoint to its per-edge stroke weights, or
/// `None` to leave it to the font glyph. Covers the pure light / heavy
/// straights, corners, tees and crosses, rounded corners (drawn as
/// sharp light corners), and the dashed variants (drawn solid so they
/// still connect). Mixed light/heavy junctions, double lines and
/// diagonals are intentionally omitted.
#[must_use]
pub const fn parts(ch: char) -> Option<BoxParts> {
    let p = match ch {
        // Straights — light + light-dashed, then heavy + heavy-dashed.
        '─' | '┄' | '┈' | '╌' => BoxParts::h(1),
        '━' | '┅' | '┉' | '╍' => BoxParts::h(2),
        '│' | '┆' | '┊' | '╎' => BoxParts::v(1),
        '┃' | '┇' | '┋' | '╏' => BoxParts::v(2),
        // Corners — light (incl. rounded ╭╮╯╰) then heavy.
        '┌' | '╭' => BoxParts::new(0, 1, 1, 0),
        '┐' | '╮' => BoxParts::new(0, 0, 1, 1),
        '└' | '╰' => BoxParts::new(1, 1, 0, 0),
        '┘' | '╯' => BoxParts::new(1, 0, 0, 1),
        '┏' => BoxParts::new(0, 2, 2, 0),
        '┓' => BoxParts::new(0, 0, 2, 2),
        '┗' => BoxParts::new(2, 2, 0, 0),
        '┛' => BoxParts::new(2, 0, 0, 2),
        // Tees — light then heavy.
        '├' => BoxParts::new(1, 1, 1, 0),
        '┤' => BoxParts::new(1, 0, 1, 1),
        '┬' => BoxParts::new(0, 1, 1, 1),
        '┴' => BoxParts::new(1, 1, 0, 1),
        '┣' => BoxParts::new(2, 2, 2, 0),
        '┫' => BoxParts::new(2, 0, 2, 2),
        '┳' => BoxParts::new(0, 2, 2, 2),
        '┻' => BoxParts::new(2, 2, 0, 2),
        // Crosses.
        '┼' => BoxParts::new(1, 1, 1, 1),
        '╋' => BoxParts::new(2, 2, 2, 2),
        _ => return None,
    };
    Some(p)
}

/// Build the filled bars for a glyph in a `cell_w` × `cell_h` cell.
///
/// Each present edge is a bar from the cell centre to that edge, so a
/// `─` (left+right) spans the full width and meets its neighbours, and
/// `┼` is a full cross. Bars overlap at the centre by their own
/// thickness, which renders the junctions cleanly.
///
/// Returns a fixed array + count (a glyph has at most 4 bars): the
/// only production caller runs this per box cell per painted frame —
/// a table border row of 200 `─` cells was 200 Vec mallocs per frame.
#[must_use]
pub fn rects(parts: BoxParts, cell_w: f32, cell_h: f32) -> ([Rect; 4], usize) {
    // Light stroke ≈ 1/12 of the cell height, heavy ≈ double, each at
    // least 1px / 2px so they never vanish on small cells.
    let light = (cell_h / 12.0).round().max(1.0);
    let heavy = (light * 2.0).max(2.0);
    let weight_px = |w: Weight| match w {
        1 => light,
        2 => heavy,
        _ => 0.0,
    };
    let cx = cell_w / 2.0;
    let cy = cell_h / 2.0;
    let mut out = [Rect::default(); 4];
    let mut n = 0;
    let mut push = |r: Rect| {
        out[n] = r;
        n += 1;
    };

    // Horizontal bars: thickness centred on cy, spanning centre→edge.
    let h_left = weight_px(parts.left);
    if h_left > 0.0 {
        push(Rect {
            x: 0.0,
            y: cy - h_left / 2.0,
            w: cx + h_left / 2.0,
            h: h_left,
        });
    }
    let h_right = weight_px(parts.right);
    if h_right > 0.0 {
        push(Rect {
            x: cx - h_right / 2.0,
            y: cy - h_right / 2.0,
            w: cell_w - (cx - h_right / 2.0),
            h: h_right,
        });
    }
    // Vertical bars: thickness centred on cx, spanning centre→edge.
    let v_up = weight_px(parts.up);
    if v_up > 0.0 {
        push(Rect {
            x: cx - v_up / 2.0,
            y: 0.0,
            w: v_up,
            h: cy + v_up / 2.0,
        });
    }
    let v_down = weight_px(parts.down);
    if v_down > 0.0 {
        push(Rect {
            x: cx - v_down / 2.0,
            y: cy - v_down / 2.0,
            w: v_down,
            h: cell_h - (cy - v_down / 2.0),
        });
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real box-drawing characters straight out of the table Claude Code
    // renders: ┌─┬─┐ / ├─┼─┤ / └─┴─┘ plus the verticals.
    const TABLE_CHARS: &[char] = &['┌', '─', '┬', '┐', '│', '├', '┼', '┤', '└', '┴', '┘'];

    #[test]
    fn every_table_char_is_handled() {
        for &ch in TABLE_CHARS {
            assert!(parts(ch).is_some(), "table char {ch:?} must render geometrically");
        }
    }

    #[test]
    fn weights_match_real_glyphs() {
        assert_eq!(parts('─'), Some(BoxParts::new(0, 1, 0, 1)));
        assert_eq!(parts('│'), Some(BoxParts::new(1, 0, 1, 0)));
        assert_eq!(parts('┼'), Some(BoxParts::new(1, 1, 1, 1)));
        assert_eq!(parts('┌'), Some(BoxParts::new(0, 1, 1, 0)));
        assert_eq!(parts('┘'), Some(BoxParts::new(1, 0, 0, 1)));
        assert_eq!(parts('┬'), Some(BoxParts::new(0, 1, 1, 1)));
        assert_eq!(parts('├'), Some(BoxParts::new(1, 1, 1, 0)));
        // Heavy + rounded + dashed map onto the same model.
        assert_eq!(parts('━'), Some(BoxParts::new(0, 2, 0, 2)));
        assert_eq!(parts('╭'), parts('┌'), "rounded corner drawn as light corner");
        assert_eq!(parts('┄'), parts('─'), "dashed drawn solid so it still connects");
        // Unhandled: double lines, diagonals, ordinary text.
        assert_eq!(parts('═'), None);
        assert_eq!(parts('╱'), None);
        assert_eq!(parts('x'), None);
    }

    /// Test-side view of [`rects`]'s fixed-array return as a slice.
    fn bars(ch: char, w: f32, h: f32) -> Vec<Rect> {
        let (arr, n) = rects(parts(ch).unwrap(), w, h);
        arr[..n].to_vec()
    }

    #[test]
    fn horizontal_bar_spans_full_cell_width() {
        // The actual fix: a `─` must reach both cell edges so adjacent
        // `─` cells join into a continuous line (no dashed gaps).
        let r = bars('─', 10.0, 20.0);
        let min_x = r.iter().map(|q| q.x).fold(f32::INFINITY, f32::min);
        let max_x = r.iter().map(|q| q.x + q.w).fold(f32::NEG_INFINITY, f32::max);
        assert!((min_x - 0.0).abs() < 0.01, "left edge reached, got {min_x}");
        assert!((max_x - 10.0).abs() < 0.01, "right edge reached, got {max_x}");
        // …and it sits on the vertical centre line.
        for q in &r {
            assert!(q.y < 10.0 && q.y + q.h > 10.0, "bar crosses cell mid-height");
        }
    }

    #[test]
    fn vertical_bar_spans_full_cell_height() {
        let r = bars('│', 10.0, 20.0);
        let min_y = r.iter().map(|q| q.y).fold(f32::INFINITY, f32::min);
        let max_y = r.iter().map(|q| q.y + q.h).fold(f32::NEG_INFINITY, f32::max);
        assert!((min_y - 0.0).abs() < 0.01, "top edge reached, got {min_y}");
        assert!((max_y - 20.0).abs() < 0.01, "bottom edge reached, got {max_y}");
    }

    #[test]
    fn corner_only_reaches_its_two_edges() {
        // `┌` = right + down only: nothing crosses the left or top edge.
        let r = bars('┌', 10.0, 20.0);
        let min_x = r.iter().map(|q| q.x).fold(f32::INFINITY, f32::min);
        let max_x = r.iter().map(|q| q.x + q.w).fold(f32::NEG_INFINITY, f32::max);
        let max_y = r.iter().map(|q| q.y + q.h).fold(f32::NEG_INFINITY, f32::max);
        assert!((max_x - 10.0).abs() < 0.01, "extends to right edge");
        assert!((max_y - 20.0).abs() < 0.01, "extends to bottom edge");
        // Left half not covered (bar starts at centre, not x=0).
        assert!(min_x > 1.0, "does not reach the left edge, got {min_x}");
    }

    #[test]
    fn heavy_is_thicker_than_light() {
        let light = bars('─', 10.0, 24.0)[0].h;
        let heavy = bars('━', 10.0, 24.0)[0].h;
        assert!(heavy > light, "heavy {heavy} should exceed light {light}");
    }
}
