// SPDX-License-Identifier: MPL-2.0

//! Cutting a long string down to its tail, safely.
//!
//! Two places keep the end of something and drop the start: the `Bash` tool, whose output the
//! model reads, and the REPL's job runner, whose output the operator reads. Both want the same
//! thing — the last stretch of a string, beginning on a line — and both are easy to get wrong in
//! the same way, so the arithmetic lives here once.

/// The byte offset to keep from: at most `max` bytes before the end, moved to the start of a line.
///
/// Zero when the whole string fits, which is the caller's signal that there is nothing to cut.
///
/// The offset is a byte count while a string is made of characters, so it lands wherever it likes,
/// and slicing inside a multi-byte character is a **panic** rather than a shortened string:
/// `String::split_off` asserts `is_char_boundary` outright, and `String::drain` asserts the same of
/// both ends. Output carrying an accent, a box-drawing character or an emoji near the cut is the
/// ordinary case rather than the exotic one — this panicked a live session while the model was
/// grepping a codebase, and the tool that ran the command was the `Bash` tool.
///
/// Moved on to a line boundary as well, so what is kept begins with a whole line: a tail that opens
/// mid-word reads as though the command printed something strange.
///
/// The result is always a valid index for a slice, a `split_off` or a `drain` of `text`.
#[must_use]
pub fn tail_start(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return 0;
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    // The first line break at or after the cut. No break means the cut already sits inside the last
    // line, which is a fine place to start — and `map_or` says that without a branch.
    text[start..].find('\n').map_or(start, |at| start + at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_that_fits_is_kept_whole() {
        assert_eq!(tail_start("short", 100), 0);
        assert_eq!(tail_start("", 0), 0);
        // Exactly the cap is not over it, so nothing is dropped.
        assert_eq!(tail_start("exact", 5), 0);
    }

    #[test]
    fn the_kept_tail_is_the_end_and_begins_a_line() {
        let text = "one\ntwo\nthree\nfour\n";
        let start = tail_start(text, 10);
        assert_eq!(&text[start..], "four\n", "the tail should start at a line");
        // A cut that lands exactly on a line start keeps that line rather than the one after it.
        assert_eq!(&text[tail_start(text, 10)..], "four\n");
    }

    /// The panic this function exists for.
    #[test]
    fn a_cut_inside_a_multibyte_character_moves_to_a_boundary() {
        // Twenty bytes of ten two-byte characters, so a cut five from the end lands inside one.
        let text = "\u{e9}".repeat(10);
        assert_eq!(text.len(), 20);
        let start = tail_start(&text, 5);
        assert!(text.is_char_boundary(start), "the offset must be a character boundary");
        // Slicing here is what panicked when the offset was used raw.
        assert_eq!(&text[start..], "\u{e9}\u{e9}");
        assert!(start >= text.len() - 5, "and it must still be a tail");
    }

    #[test]
    fn a_boundary_is_found_whatever_the_character_width() {
        // Four-byte characters, so the cut can land at any of four offsets inside one.
        let text = "\u{1F600}".repeat(8); // 32 bytes
        for max in 1..=8 {
            let start = tail_start(&text, max);
            assert!(text.is_char_boundary(start), "max {max} produced {start}");
            assert!(text.len() - start <= max, "max {max} kept more than it should");
        }
    }

    #[test]
    fn a_cut_with_no_line_break_after_it_is_left_where_it_is() {
        // No newline at all: the boundary is the best there is, and the caller's slice still works.
        let text = "x".repeat(100);
        let start = tail_start(&text, 10);
        assert_eq!(start, 90);
        assert_eq!(&text[start..], "x".repeat(10));
    }
}
