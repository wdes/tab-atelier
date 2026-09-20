// SPDX-License-Identifier: MPL-2.0

//! A small markdown renderer, for answers the model was asked to write in markdown.
//!
//! Not a markdown implementation. It handles the subset the instruction asks for —
//! headings, emphasis, inline code, fenced code, and pipe tables — and passes
//! everything else through unchanged, because an unrecognised line is still
//! something the operator can read and a wrong guess is not.
//!
//! The tables are the part that matters, and the part with a real requirement
//! behind it: they have to **render aligned and copy-paste as markdown**. Those
//! pull in opposite directions — box-drawing characters render beautifully and
//! paste as mojibake, and raw markdown pastes perfectly and renders as a ragged
//! wall of pipes. Padding the cells and keeping the pipes satisfies both: the
//! columns line up on screen, and what you get when you select it is a valid
//! markdown table with the same cell values.
//!
//! ANSI escapes are removed first. The model is asked not to emit them and the
//! agent strips them on the way out, but a renderer that assumes its input is
//! clean is a renderer that shows `[1m` the first time that assumption breaks.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ansi;

/// Render markdown into styled lines.
///
/// The result is what the REPL prints, and its text content is what a reader would
/// copy: styling is carried in spans, never in the characters.
#[must_use]
pub fn render(input: &str) -> Vec<Line<'static>> {
    let clean = ansi::strip(input);
    let lines: Vec<&str> = clean.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // A fenced block: passed through verbatim, because code that has been
        // re-flowed or emphasised is no longer the code that was written.
        if is_fence(line) {
            out.push(Line::from(Span::styled(
                "─── code ───".to_owned(),
                Style::default().fg(Color::DarkGray),
            )));
            i += 1;
            while i < lines.len() && !is_fence(lines[i]) {
                out.push(Line::from(Span::raw(lines[i].to_owned())));
                i += 1;
            }
            // The closing fence, or the end of the input.
            i += 1;
            continue;
        }

        // A table: a pipe row followed by a rule row.
        if is_table_row(line) && lines.get(i + 1).is_some_and(|next| is_rule_row(next)) {
            let (rendered, consumed) = render_table(&lines[i..]);
            out.extend(rendered);
            i += consumed;
            continue;
        }

        if let Some(rest) = heading(line) {
            out.push(Line::from(Span::styled(
                rest.to_owned(),
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
            )));
            i += 1;
            continue;
        }

        out.push(Line::from(inline(line)));
        i += 1;
    }
    out
}

/// Whether a line is a code fence.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// A heading, returned without its marks.
///
/// The hashes are dropped rather than kept: they render as noise and the styling
/// carries the same information, and a heading is not something anyone copies out
/// of an answer.
fn heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    // A heading needs a space after the hashes, or `#1` in a sentence becomes one.
    rest.strip_prefix(' ').map(str::trim_end)
}

/// Whether a line is a table row: it starts and ends with a pipe.
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() > 1
}

/// Whether a line is the rule under a table's header: pipes, dashes and colons.
fn is_rule_row(line: &str) -> bool {
    let t = line.trim();
    if !is_table_row(t) {
        return false;
    }
    let middle = &t[1..t.len() - 1];
    middle.chars().any(|c| c == '-') && middle.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
}

/// Split a table row into its cells.
fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_owned()).collect()
}

/// Render a pipe table as an aligned pipe table.
///
/// Returns the lines and how many input lines were consumed.
fn render_table(lines: &[&str]) -> (Vec<Line<'static>>, usize) {
    let header = cells(lines[0]);
    // `lines[1]` is the rule, which is replaced by a rule of the computed width.
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut used = 2;
    while let Some(line) = lines.get(used) {
        if !is_table_row(line) || is_rule_row(line) {
            break;
        }
        rows.push(cells(line));
        used += 1;
    }

    let width = header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if width == 0 {
        return (vec![Line::from(lines[0].to_owned())], 1);
    }

    // Column widths, measured in characters so a wide glyph counts once. A cell
    // holding a multi-byte character is why `len()` alone is not enough.
    let mut widths = vec![0usize; width];
    for row in std::iter::once(&header).chain(rows.iter()) {
        for (col, cell) in row.iter().enumerate() {
            if col < width {
                widths[col] = widths[col].max(display_width(cell));
            }
        }
    }

    // A column of numbers reads better right-aligned, and the decision is made
    // from the *data* rows: a header saying "Count" is not numeric itself, and
    // aligning the header the other way from its column looks like a mistake.
    let numeric: Vec<bool> = (0..width)
        .map(|col| {
            let data: Vec<&String> = rows.iter().filter_map(|r| r.get(col)).collect();
            !data.is_empty() && data.iter().all(|c| is_number(c))
        })
        .collect();

    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(table_line(&header, &widths, &numeric, Some(Modifier::BOLD)));
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w + 2)).collect();
    out.push(Line::from(Span::styled(
        format!("|{}|", rule.join("|")),
        Style::default().fg(Color::DarkGray),
    )));
    for row in &rows {
        out.push(table_line(row, &widths, &numeric, None));
    }
    (out, used)
}

/// One padded, styled row.
fn table_line(row: &[String], widths: &[usize], numeric: &[bool], extra: Option<Modifier>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.len() * 3);
    spans.push(Span::raw("|"));
    for (col, width) in widths.iter().enumerate() {
        let cell = row.get(col).cloned().unwrap_or_default();
        let pad = width.saturating_sub(display_width(&cell));
        let body = if numeric.get(col).copied().unwrap_or(false) {
            format!(" {}{} ", " ".repeat(pad), cell)
        } else {
            format!(" {}{} ", cell, " ".repeat(pad))
        };
        let style = extra.map_or_else(Style::default, |m| Style::default().add_modifier(m));
        spans.push(Span::styled(body, style));
        spans.push(Span::raw("|"));
    }
    Line::from(spans)
}

/// A cell that is a number, for alignment purposes.
///
/// Deliberately narrow: digits, one separator, a sign or a trailing percent. A
/// cell like `1,234` or `-5.2%` counts; `v2` does not, or a column of version
/// strings would be right-aligned.
fn is_number(cell: &str) -> bool {
    let body = cell.strip_suffix('%').unwrap_or(cell);
    let body = body.strip_prefix(['-', '+']).unwrap_or(body);
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == '_')
        && body.chars().any(|c| c.is_ascii_digit())
}

/// Inline styling: `**bold**` and `` `code` ``.
///
/// Nothing is removed from the text except the markers themselves, so a line
/// without any comes through as one span and costs nothing extra.
fn inline(line: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        // `**bold**`
        if chars[i] == '*'
            && chars.get(i + 1) == Some(&'*')
            && let Some(end) = find(&chars, i + 2, &['*', '*'])
        {
            push_plain(&mut spans, &mut plain);
            let body: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(body, Style::default().add_modifier(Modifier::BOLD)));
            i = end + 2;
            continue;
        }
        // `` `code` ``
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|c| *c == '`').map(|p| p + i + 1)
        {
            push_plain(&mut spans, &mut plain);
            let body: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(body, Style::default().fg(Color::Yellow)));
            i = end + 1;
            continue;
        }
        plain.push(chars[i]);
        i += 1;
    }
    push_plain(&mut spans, &mut plain);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Flush the pending plain text into a span, if it holds anything.
fn push_plain(spans: &mut Vec<Span<'static>>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::raw(std::mem::take(plain)));
    }
}

/// The next index where `needle` starts, from `from`.
fn find(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..chars.len().saturating_sub(needle.len() - 1)).find(|i| chars.get(*i..*i + needle.len()) == Some(needle))
}

/// A string's width in characters.
///
/// Characters, not bytes and not grapheme clusters: this is used for padding, and
/// a width that disagrees with the terminal by one cell is a worse failure than
/// one that is merely approximate for an emoji.
fn display_width(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of rendered lines, as a reader copying them would get it.
    fn copied(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const TABLE: &str = "\
Results:

| Name | Count | Note |
|------|-------|------|
| alpha | 1 | first |
| beta | 22 | second, longer |
| gamma | 333 | third |

That is all.";

    /// The requirement in one test: a table has to be readable on screen *and*
    /// survive a copy-paste as markdown.
    #[test]
    fn a_table_renders_aligned_and_pastes_back_as_markdown() {
        let rendered = render(TABLE);
        let text = copied(&rendered);
        let rows: Vec<&str> = text.lines().filter(|l| l.trim_start().starts_with('|')).collect();

        assert_eq!(rows.len(), 5, "header, rule and three data rows: {text}");

        // Aligned on screen: every pipe is in the same column in every row, which
        // is the whole visual difference between a table and a wall of pipes.
        let positions: Vec<Vec<usize>> = rows
            .iter()
            .map(|row| row.char_indices().filter(|(_, c)| *c == '|').map(|(i, _)| i).collect())
            .collect();
        for (i, row) in positions.iter().enumerate().skip(1) {
            assert_eq!(
                row.len(),
                positions[0].len(),
                "row {i} has a different number of cells:\n{text}"
            );
            assert_eq!(*row, positions[0], "row {i} is not aligned with the header:\n{text}");
        }

        // Pastes back as markdown: the pipes and the rule are still pipes and a
        // rule, so what lands in a paste buffer is a valid table.
        assert!(rows[1].contains("---"), "the rule row survives: {text}");
        for row in &rows {
            let cells = row.matches('|').count();
            assert_eq!(cells, 4, "three columns means four pipes: {row}");
        }
        // And the values are all still there, not truncated by padding.
        for value in [
            "Name",
            "Count",
            "Note",
            "alpha",
            "beta",
            "gamma",
            "333",
            "second, longer",
        ] {
            assert!(text.contains(value), "{value:?} went missing from:\n{text}");
        }
        // The prose around the table is untouched.
        assert!(text.starts_with("Results:"), "{text}");
        assert!(text.trim_end().ends_with("That is all."), "{text}");
    }

    /// A column of numbers is right-aligned, because that is what makes a column of
    /// numbers readable; the header follows the data it labels.
    #[test]
    fn a_numeric_column_is_right_aligned_including_its_header() {
        let rendered = render("| Item | Amount |\n|---|---|\n| one | 5 |\n| two | 1234 |");
        let text = copied(&rendered);
        let rows: Vec<&str> = text.lines().collect();
        // "1234" is wider than the header, so the header must be pushed right.
        let amount_header = rows[0].rfind("Amount").expect("header");
        let amount_value = rows[3].rfind("1234").expect("the data row, not the rule");
        assert_eq!(
            amount_header + "Amount".len(),
            amount_value + "1234".len(),
            "the numbers and their header must share a right edge:\n{text}"
        );
    }

    /// A column of version strings is not a numeric column, or `v2` and `v10` would
    /// be right-aligned to look like quantities.
    #[test]
    fn a_column_of_non_numbers_is_not_right_aligned() {
        let rendered = render("| Ver |\n|---|\n| v2 |\n| v10 |");
        let text = copied(&rendered);
        let rows: Vec<&str> = text.lines().collect();
        let v2 = rows[2].find("v2").expect("first data row, not the rule");
        let v10 = rows[3].find("v10").expect("second data row");
        assert_eq!(v2, v10, "non-numeric cells are left-aligned:\n{text}");
    }

    /// Escapes must never reach a reader, whatever the model emitted.
    #[test]
    fn escapes_are_removed() {
        let rendered = render("\u{1b}[1;36mHeader\u{1b}[0m\n\n| a |\n|---|\n| \u{1b}[32m1\u{1b}[0m |");
        let text = copied(&rendered);
        assert!(!text.contains('\u{1b}'), "no escape survives: {text:?}");
        assert!(!text.contains("[1;36m"), "and no escape body either: {text:?}");
        assert!(text.contains("Header"), "{text}");
        assert!(text.contains('1'), "{text}");
    }

    #[test]
    fn headings_lose_their_marks_and_gain_emphasis() {
        let rendered = render("## Findings\n\ntext");
        let first = &rendered[0];
        assert_eq!(first.spans[0].content.as_ref(), "Findings", "the hashes are gone");
        assert!(
            first.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "and the emphasis carries the meaning instead"
        );
        // `#1` in prose is not a heading: a heading needs the space.
        let rendered = render("#1 raised in review");
        assert_eq!(copied(&rendered), "#1 raised in review");
    }

    #[test]
    fn inline_emphasis_and_code_are_styled_without_their_markers() {
        let rendered = render("use **bold** and `code` here");
        let text = copied(&rendered);
        assert_eq!(text, "use bold and code here");
        assert!(!text.contains("**"), "the markers are gone: {text}");
        assert!(!text.contains('`'), "the backticks are gone: {text}");
        let spans = &rendered[0].spans;
        assert!(spans.iter().any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans.iter().any(|s| s.style.fg == Some(Color::Yellow)));
    }

    /// An unclosed marker is left as written rather than swallowing the rest of the
    /// line: a model that emits one stray asterisk should not lose its sentence.
    #[test]
    fn an_unclosed_marker_is_left_alone() {
        assert_eq!(copied(&render("a **b")), "a **b");
        assert_eq!(copied(&render("a `b")), "a `b");
    }

    #[test]
    fn a_code_fence_is_preserved_verbatim() {
        let rendered = render("before\n```\nlet x = **not bold**;\n```\nafter");
        let text = copied(&rendered);
        assert!(text.contains("let x = **not bold**;"), "code is not re-styled: {text}");
        assert!(text.starts_with("before"), "{text}");
        assert!(text.trim_end().ends_with("after"), "{text}");
    }

    /// A pipe row with no rule under it is prose, not a table. Guessing otherwise
    /// would mangle a sentence that happens to use a pipe.
    #[test]
    fn a_pipe_row_without_a_rule_is_not_a_table() {
        let rendered = render("| not a table |\njust text");
        let text = copied(&rendered);
        assert!(text.starts_with("| not a table |"), "left as written: {text}");
    }

    /// A table missing a cell is padded rather than dropped, so a ragged table still
    /// renders as a table instead of crashing or collapsing.
    #[test]
    fn a_ragged_table_is_padded_to_its_widest_row() {
        let rendered = render("| a | b |\n|---|---|\n| only-a |\n| x | y |");
        let text = copied(&rendered);
        for row in text.lines().filter(|l| l.starts_with('|')) {
            assert_eq!(row.matches('|').count(), 3, "every row has two columns: {row}");
        }
        assert!(text.contains("only-a"), "{text}");
    }

    /// The width measurement is in characters, so a row with a multi-byte glyph
    /// still aligns with its neighbours.
    #[test]
    fn a_multibyte_cell_still_aligns() {
        let rendered = render("| name | n |\n|---|---|\n| café | 1 |\n| plain | 22 |");
        let text = copied(&rendered);
        let rows: Vec<&str> = text.lines().filter(|l| l.starts_with('|')).collect();
        // Character positions, not byte offsets: the padding is a character count, so
        // a byte index shifts by one on `café` and would report a misalignment that is
        // not on the screen. The first attempt at this test used `char_indices` and
        // failed on its own, which is the bug this comment is here to stop recurring.
        let pipes = |row: &str| -> Vec<usize> {
            row.chars()
                .enumerate()
                .filter(|(_, c)| *c == '|')
                .map(|(i, _)| i)
                .collect()
        };
        for row in &rows[1..] {
            assert_eq!(pipes(row), pipes(rows[0]), "columns should line up:\n{text}");
        }
    }
}
