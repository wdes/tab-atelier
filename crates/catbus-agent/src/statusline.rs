// SPDX-License-Identifier: MPL-2.0

//! The two lines the REPL shows about a turn.
//!
//! While the agent works, one line is repainted in place: a spinner, the current
//! activity, and an estimate of the input tokens this request will cost. When
//! the reply lands, a totals line is printed below it — cumulative tokens on the
//! left, the gate mode on the right.
//!
//! The visible count during a download is necessarily an *estimate*, marked with
//! `~`: the Messages API reports `usage` only in its final response, so until
//! then the only figure available is local arithmetic on the payload size. The
//! authoritative numbers appear on the totals line, which is fed from the
//! server's own `usage`. [`estimate_input_tokens`] carries the reasoning.
//!
//! Both lines are *pure formatting* on purpose. The REPL itself is only
//! reachable through a real terminal, so anything that can be a function taking
//! numbers is a function here, where the edge cases — a terminal narrower than
//! the text, a count with separators, a missing estimate, the mode changing
//! mid-session — can be tested without a tty.

/// Spinner frames — simple ASCII so any font renders them.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Width assumed when the terminal size cannot be read — piped output, or a pty
/// that never got a size. 80 because it is the width every terminal honours.
const FALLBACK_WIDTH: usize = 80;

/// The label used while waiting on the model, before any tool is involved.
///
/// The agent's status is the lower-case `"thinking"`; the REPL presents it
/// capitalised, and [`spinner_line`] compares against this constant rather than
/// a literal so the two cannot drift.
pub const THINKING: &str = "Thinking";

/// `1234567` → `1,234,567`.
///
/// Token counts reach six digits on a long session, where an unseparated number
/// is genuinely hard to read at a glance — which is the whole point of putting
/// it on screen.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        // A separator every three digits from the right, and never leading.
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1,234 in - 567 out` — the left-hand field of the totals line.
#[must_use]
pub fn tokens_label(tokens_in: u64, tokens_out: u64) -> String {
    format!("{} in - {} out", thousands(tokens_in), thousands(tokens_out))
}

/// The gate, as the operator thinks of it.
///
/// Two states, not three: the question this answers is "will the judge be
/// consulted before something changes?", which is what distinguishes auto mode.
/// `plan` is a *manual* mode in that sense — nothing runs without the operator —
/// so it reads as `manual` alongside `open`. The finer distinction is already
/// visible in the environment turn and in `/help`.
#[must_use]
pub const fn mode_label(gate: crate::tools::Gate) -> &'static str {
    match gate {
        crate::tools::Gate::Auto => "auto",
        crate::tools::Gate::Open | crate::tools::Gate::Plan => "manual",
    }
}

/// The totals line: tokens left, mode right, padded to `width`.
///
/// Right-alignment is done by padding rather than a cursor-move escape, so the
/// line is correct in a log or a `script` capture too — the escapes in
/// [`spinner_line`] are unavoidable there because it *repaints*, but this line is
/// written once and should survive being copied.
#[must_use]
pub fn totals_line(tokens_in: u64, tokens_out: u64, gate: crate::tools::Gate, width: usize) -> String {
    let left = tokens_label(tokens_in, tokens_out);
    let right = mode_label(gate);
    // At least one space, so the two fields never touch. On a terminal narrower
    // than the text that means the line overflows and wraps rather than being
    // silently cut — a wrapped status line is legible, a truncated one lies
    // about the numbers.
    let pad = width
        .saturating_sub(left.chars().count() + right.chars().count())
        .max(1);
    format!("{left}{}{right}", " ".repeat(pad))
}

/// Rough bytes per token for English text under a BPE tokenizer.
///
/// An approximation, and named as one: the real count comes from the server's
/// `usage.input_tokens` and is shown on the totals line once the reply lands.
pub const BYTES_PER_TOKEN: usize = 4;

/// Estimate the input tokens a request of `bytes` will cost.
///
/// Needed because the Messages API reports `usage` only in its *final* response,
/// so while a request is in flight there is no authoritative figure to show. The
/// spinner would otherwise read `0 tokens in` for the whole download — which is
/// what it did before this existed — and a number that never moves is worse than
/// no number, since it looks like the request is stuck.
///
/// Deliberately crude. A byte-length ratio is wrong by a wide margin on
/// non-English text, code, and JSON scaffolding, and it is still the right
/// choice here: the alternative is either a tokenizer dependency (large, and
/// wrong for whatever model the relay actually routes to) or a spinner that
/// shows nothing. `~/` in the display marks it as an estimate so it cannot be
/// mistaken for the server's own count.
#[must_use]
pub const fn estimate_input_tokens(bytes: usize) -> u64 {
    (bytes / BYTES_PER_TOKEN) as u64
}

/// The live line, with the leading `\r\x1b[K` that repaints the previous frame.
///
/// `tokens` is the in-flight estimate, or `None` before a request has been
/// serialised — in which case the count is omitted rather than shown as `~0`,
/// because `~0 tokens in` claims a measurement of zero when in fact nothing has
/// been measured yet.
///
/// `\r` parks the cursor at column 0 and `\x1b[K` erases to end-of-line, so a
/// shorter line never leaves the tail of a longer one behind — the same reason
/// the spinner already used them when the activity could change from a long tool
/// name to the short word "thinking".
#[must_use]
pub fn spinner_line(frame: usize, activity: &str, tokens: Option<u64>) -> String {
    let spinner = SPINNER[frame % SPINNER.len()];
    // The `~` marks this as the local estimate, not the server's count. An
    // absent estimate yields an empty string: see the doc comment for why it is
    // omitted rather than rendered as `~0`.
    let count = tokens.map_or_else(String::new, |n| format!(" - ~{} tokens in", thousands(n)));
    format!("\r\x1b[K\x1b[36m{spinner}\x1b[0m {activity}{count}")
}

/// The activity text for a spinner frame: the agent's status, presented.
///
/// The agent reports `"thinking"` lower-case; every other status is a tool
/// description it wrote for exactly this purpose, so it is passed through as-is.
#[must_use]
pub fn activity_label(status: &str) -> String {
    if status == "thinking" {
        THINKING.to_owned()
    } else {
        status.to_owned()
    }
}

/// The terminal's width in columns, or [`FALLBACK_WIDTH`].
///
/// A pty can report 0 columns before anything sets a size, and a zero would pad
/// the totals line to nothing, so that case falls back too.
#[must_use]
pub fn terminal_width() -> usize {
    terminal_size::terminal_size().map_or(
        FALLBACK_WIDTH,
        |(w, _)| {
            if w.0 == 0 { FALLBACK_WIDTH } else { usize::from(w.0) }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Gate;

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        // The boundary the grouping must get right: no separator until the
        // fourth digit, then one after every third.
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234), "1,234");
        assert_eq!(thousands(12_345), "12,345");
        assert_eq!(thousands(123_456), "123,456");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn tokens_label_names_both_directions() {
        assert_eq!(tokens_label(1_234, 567), "1,234 in - 567 out");
        assert_eq!(tokens_label(0, 0), "0 in - 0 out");
    }

    #[test]
    fn only_auto_mode_reads_as_auto() {
        // The distinction the label exists to make: is the judge in the loop?
        assert_eq!(mode_label(Gate::Auto), "auto");
        assert_eq!(mode_label(Gate::Open), "manual");
        assert_eq!(mode_label(Gate::Plan), "manual");
    }

    #[test]
    fn the_totals_line_fills_the_width_exactly() {
        let line = totals_line(1_234, 567, Gate::Auto, 80);
        assert_eq!(line.chars().count(), 80, "{line:?}");
        assert!(line.starts_with("1,234 in - 567 out"), "{line:?}");
        assert!(line.ends_with("auto"), "{line:?}");
        // Nothing but padding between them.
        assert_eq!(line.trim_end().len(), line.len());
    }

    #[test]
    fn the_totals_line_right_aligns_the_mode_at_any_width() {
        // The visible property: the mode ends at the last column, whatever the
        // width, so a resized terminal keeps it flush rather than drifting.
        for width in [40, 60, 80, 120, 200] {
            let line = totals_line(12_345, 6_789, Gate::Open, width);
            assert_eq!(line.chars().count(), width, "width {width}: {line:?}");
            assert!(line.ends_with("manual"), "width {width}: {line:?}");
            assert!(line.starts_with("12,345 in - 6,789 out"), "width {width}: {line:?}");
        }
    }

    #[test]
    fn a_terminal_too_narrow_keeps_both_fields_separated() {
        // Degrade to one space rather than truncating: a wrapped line is
        // legible, a cut one misreports the numbers.
        let line = totals_line(1_234, 567, Gate::Auto, 4);
        assert_eq!(line, "1,234 in - 567 out auto");
        // And the two fields are still both present and correctly labelled.
        assert!(line.contains(" in - "));
        assert!(line.ends_with("auto"));
    }

    #[test]
    fn the_spinner_line_names_the_activity_and_the_count() {
        let line = spinner_line(0, THINKING, Some(1_234));
        assert!(line.contains("Thinking"), "{line:?}");
        assert!(line.contains("~1,234 tokens in"), "{line:?}");
        // The dash is what the format is specified with.
        assert!(line.contains("Thinking - ~1,234 tokens in"), "{line:?}");
    }

    #[test]
    fn the_count_is_marked_as_an_estimate() {
        // The `~` is the whole honesty story for this field: the server reports
        // `usage` only in its final response, so anything shown *during* the
        // download is local arithmetic on the payload length. Without the marker
        // it reads as an authoritative figure.
        let line = spinner_line(0, THINKING, Some(99));
        assert!(line.contains('~'), "an estimate must be marked: {line:?}");
    }

    #[test]
    fn no_estimate_omits_the_count_rather_than_showing_zero() {
        // `~0 tokens in` would claim a measurement of zero when nothing has been
        // measured yet. Better to say nothing about tokens at all.
        let line = spinner_line(0, THINKING, None);
        assert!(line.contains("Thinking"), "{line:?}");
        assert!(!line.contains("tokens"), "no count should be shown: {line:?}");
        assert!(!line.contains('~'), "{line:?}");
        // Still a valid repaint line.
        assert!(line.starts_with("\r\x1b[K"), "{line:?}");
    }

    #[test]
    fn the_estimate_scales_with_payload_size() {
        // A rough ratio, but it must be monotonic and sane: a bigger payload
        // never estimates fewer tokens, and the order of magnitude holds.
        assert_eq!(estimate_input_tokens(0), 0);
        assert_eq!(estimate_input_tokens(4), 1);
        assert_eq!(estimate_input_tokens(4_000), 1_000);
        let small = estimate_input_tokens(1_000);
        let large = estimate_input_tokens(100_000);
        assert!(large > small, "estimate is not monotonic");
        assert_eq!(large, small * 100);
    }

    #[test]
    fn the_estimate_is_not_mistaken_for_zero_on_a_real_payload() {
        // The regression this whole estimate exists for: before it, the spinner
        // read "0 tokens in" for an entire download, because the only source of
        // a real count is the response the spinner is waiting on. A payload of a
        // few kilobytes must produce a visible number.
        let typical_request_bytes = 30_000;
        let est = estimate_input_tokens(typical_request_bytes);
        assert!(est > 1_000, "a 30 kB request should not estimate as {est} tokens");
        assert!(spinner_line(0, THINKING, Some(est)).contains("tokens in"));
    }

    #[test]
    fn the_spinner_line_erases_the_previous_frame_first() {
        // Without these a shorter line leaves the tail of a longer one behind —
        // which is exactly how "Bash: grep -ri ..." used to bleed into
        // "thinking ... ri" before the erase was added.
        let line = spinner_line(0, THINKING, Some(0));
        assert!(line.starts_with("\r\x1b[K"), "{line:?}");
    }

    #[test]
    fn the_spinner_animates_and_wraps() {
        // Frames advance, and a large frame index must not panic — the counter is
        // never reset, so it will exceed the frame list on a long session.
        let first = spinner_line(0, THINKING, Some(0));
        let second = spinner_line(1, THINKING, Some(0));
        assert_ne!(first, second, "the spinner does not animate");
        let wrapped = spinner_line(SPINNER.len(), THINKING, Some(0));
        assert_eq!(wrapped, first, "frame {} should wrap to frame 0", SPINNER.len());
        // Far beyond the frame list, and no panic.
        let _ = spinner_line(usize::MAX, THINKING, Some(0));
    }

    #[test]
    fn a_tool_activity_is_passed_through_unchanged() {
        // The agent writes these for display, so they are shown verbatim; only
        // the lower-case status word is prettified.
        assert_eq!(activity_label("thinking"), "Thinking");
        assert_eq!(activity_label("Bash: grep -ri foo"), "Bash: grep -ri foo");
        assert_eq!(activity_label("Read: src/main.rs"), "Read: src/main.rs");
    }

    #[test]
    fn the_thinking_label_matches_what_the_agent_reports() {
        // `activity_label` compares against "thinking", which is the literal the
        // agent sets. Hard-coded in two places, so pinned here: a rename on
        // either side would otherwise surface as a spinner that reads
        // "thinking - 0 tokens in" in lower case.
        assert_eq!(THINKING, "Thinking");
        assert_eq!(activity_label("thinking"), THINKING);
    }
}
