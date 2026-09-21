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

/// `1,234 in - 567 out` — the left-hand field of the totals line.
#[must_use]
pub fn tokens_label(tokens_in: u64, tokens_out: u64) -> String {
    format!("{} in - {} out", thousands(tokens_in), thousands(tokens_out))
}

/// A count with thousands separators: `1234` becomes `1,234`.
///
/// Hand-rolled rather than pulled in: the alternative is a dependency for four
/// lines, and the separators are load-bearing here — a raw `12345` read at a glance
/// is hard to size, and these numbers are the one piece of the status line an
/// operator actually compares across turns.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        // A separator every third digit, counted from the right.
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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

/// What the session has cost, and the model it is running as.
///
/// One figure per currency, never added together: a session that used two providers billed in two
/// currencies has two totals, and a single number would be a lie about which. The currency code
/// stays on each amount because a bare number invites being read as some currency that has not been
/// named.
///
/// Two things are said rather than hidden when they apply. With no prices at all — a relay that
/// serves none, or a model the catalog does not describe — the line says `no prices` instead of
/// showing `0.00000`, because an unknown price and a price of zero mean opposite things and the
/// second is what a zero implies. And tokens no price covered are counted out loud, so a total that
/// looks low has its explanation on screen rather than inviting the reader to doubt the arithmetic.
#[must_use]
pub fn cost_line(model: Option<&str>, amounts: &[(String, f64)], unpriced: u64, width: usize) -> String {
    let mut out = String::new();
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        out.push_str(model);
    }

    let money = if amounts.is_empty() {
        if unpriced > 0 {
            // Named as unpriced rather than charged: the tokens were counted, and no rate is known
            // for them.
            format!("no price for {} tokens", crate::statusline::thousands(unpriced))
        } else {
            String::new()
        }
    } else {
        amounts
            .iter()
            .map(|(currency, amount)| format!("{currency} {amount:.5}"))
            .collect::<Vec<_>>()
            .join("  ")
    };

    if money.is_empty() {
        return out;
    }
    if !out.is_empty() {
        out.push_str("   ");
    }
    if !amounts.is_empty() && unpriced > 0 {
        // Both halves matter: what was charged, and what could not be. Built as one string and
        // pushed once rather than `push_str(&format!(..))`, which the lints prefer against.
        let note = format!("{money}  (+{} tokens unpriced)", crate::statusline::thousands(unpriced));
        out.push_str(&note);
    } else {
        out.push_str(&money);
    }

    // Wrapped rather than cut: half a cost line is worse than a long one, and the caller has
    // already put the tokens on their own line above.
    if out.chars().count() > width {
        return out;
    }
    out
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

/// The label shown while the model is thinking and has not called a tool.
pub const THINKING: &str = "Thinking";

/// The marker the agent puts in its status while it waits on the model.
///
/// Distinct from [`THINKING`], which is the label: the agent's marker is internal
/// and lower-case, and the label is what an operator reads. Both live here so the
/// comparison in [`activity_label`] has one spelling to match and `agent` has one
/// spelling to set — a rename on either side used to surface as a spinner reading
/// "thinking" in lower case.
pub const THINKING_MARKER: &str = "thinking";

/// The activity text for a spinner frame: the agent's status, presented.
///
/// The agent reports `"thinking"` lower-case while it waits on the model, and a tool description
/// once it starts calling tools. The tool description says what the *tool* is doing but not that
/// the model is still working, and a status that dropped "Thinking" made the two states look
/// unrelated — the operator lost the thread of "it is thinking, and right now it is reading this
/// file". So the tool is shown *alongside* the thinking label:
///
/// ```text
/// ⠋  Thinking
/// ⠋  Thinking - Read(src/handlers.rs)
/// ```
pub fn activity_label(status: &str) -> String {
    if status == THINKING_MARKER {
        THINKING.to_owned()
    } else {
        format!("{THINKING} - {status}")
    }
}

/// Column width assumed when the terminal will not say.
const FALLBACK_WIDTH: usize = 80;

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
        // And it is a number the display path can render.
        assert!(est.to_string().len() >= 4, "a 30 kB request showed {est}");
    }

    #[test]
    fn a_tool_activity_is_shown_beside_the_thinking_label() {
        // The tool says what is being done; "Thinking" says the model is still working on it.
        // Both are wanted, because dropping either loses information the operator was using.
        assert_eq!(activity_label("thinking"), "Thinking");
        assert_eq!(
            activity_label("Read(src/handlers.rs)"),
            "Thinking - Read(src/handlers.rs)"
        );
        assert_eq!(activity_label("Bash(cargo test)"), "Thinking - Bash(cargo test)");
        // The judge's status goes through the same composition: it is another thing the model is
        // doing while the turn runs, so it reads the same way.
        assert_eq!(activity_label("checking Bash"), "Thinking - checking Bash");
    }

    #[test]
    fn the_thinking_label_matches_what_the_agent_reports() {
        // The label and the marker are different strings on purpose, and the
        // agent sets the marker: a rename on either side would otherwise surface
        // as a spinner reading "thinking" in lower case.
        assert_eq!(THINKING, "Thinking");
        assert_eq!(THINKING_MARKER, "thinking");
        assert_eq!(activity_label(THINKING_MARKER), THINKING);
    }
}
