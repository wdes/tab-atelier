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
//!
//! The one piece of state is [`Activity`], which decides *which* activity is worth
//! showing; it takes the clock as an argument rather than reading it, so its timing
//! rules are testable too.

use std::time::{Duration, Instant};

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
pub fn cost_line(
    model: Option<&str>,
    amounts: &[(String, f64)],
    unpriced: crate::cost::Tokens,
    width: usize,
) -> String {
    let mut out = String::new();
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        out.push_str(model);
    }

    let money = if amounts.is_empty() {
        if unpriced.is_empty() {
            // Nothing was counted at all, so there is no cost to claim — the word `no price` over
            // zero tokens would read as a failure on a session that has not run a turn yet.
            String::new()
        } else {
            // Named as unpriced rather than charged: the tokens were counted, and no rate is known
            // for them.
            //
            // The cache is spelled out when there is any, because otherwise this number looks wrong
            // next to the totals line — which counts only `in - out`. On a cached session the cache
            // is most of the total, so the two figures differed by millions with nothing on screen
            // to explain it. Saying which part is cache makes the arithmetic check out: the four
            // counts here sum to this number.
            format!(
                "no price for {} tokens{}",
                crate::statusline::thousands(unpriced.total()),
                cache_note(unpriced)
            )
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
    if !amounts.is_empty() && !unpriced.is_empty() {
        // Both halves matter: what was charged, and what could not be. Built as one string and
        // pushed once rather than `push_str(&format!(..))`, which the lints prefer against.
        let note = format!(
            "{money}  (+{} tokens unpriced{})",
            crate::statusline::thousands(unpriced.total()),
            cache_note(unpriced)
        );
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

/// ", of which N cache" when any of the tokens are cache reads or writes, else nothing.
///
/// The totals line counts only input and output. Saying which part of *this* figure is cache is what
/// makes the two reconcilable on screen, instead of leaving the reader to wonder why one number is
/// several times the other. Empty when there is no cache, so the note does not appear where it would
/// explain nothing.
fn cache_note(tokens: crate::cost::Tokens) -> String {
    if tokens.cached() == 0 {
        return String::new();
    }
    format!(", of which {} cache", crate::statusline::thousands(tokens.cached()))
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

/// The glyph beside an activity that has just finished.
pub const DONE: &str = "✓";

/// How long an activity must have been running before the row names it.
///
/// A tool that returns in five milliseconds would otherwise paint its name on the row for a single
/// frame, and a run of fast tools strobes through names nobody can read. Long enough to hide the
/// fast ones, short enough that a real tool — a `Bash` that takes a second, a fetch — is named
/// almost at once.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// How long a finished activity's check stays on the row.
///
/// The check is the only signal that something *completed* rather than merely started, so it has to
/// outlast a glance; and it has to be gone before the next activity, or the row would claim two
/// things were running.
const LINGER: Duration = Duration::from_millis(600);

/// Turns the agent's status into what the row shows, with an eye on the clock and not only on the
/// text.
///
/// [`activity_label`] formats a status; this decides *which* status is worth showing. Both rules
/// are about time:
///
/// - **Debounce.** A name is held back until it has been up for [`DEBOUNCE`], and the row says
///   plain `Thinking` until then. Otherwise every fast tool flashes its name for a frame.
/// - **Lingering check.** When an activity ends the row would snap straight back to `Thinking`,
///   discarding the fact that it ended at all — so the name stays for [`LINGER`] with [`DONE`]
///   beside it. That is what makes "it read that file" distinguishable from "it has not started",
///   which in a bare spinner are the same picture.
///
/// Stateful by necessity, and `now` is passed in rather than read from the clock, so the timing
/// rules can be tested without sleeping.
#[derive(Debug, Default)]
pub struct Activity {
    /// The activity being shown, and when it first appeared. `None` when the row is between
    /// activities — which is the normal state while the model is being called.
    running: Option<(String, Instant)>,
    /// An activity that finished recently, and when, so its check can linger.
    finished: Option<(String, Instant)>,
}

impl Activity {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What the row should say for `status`, at `now`.
    pub fn label(&mut self, status: &str, now: Instant) -> String {
        if status == THINKING_MARKER {
            // The model is being called again, which means whatever was running has finished. The
            // transition is the only signal available: the agent reports the tool while it runs and
            // the bare marker afterwards, with nothing in between to say the tool returned.
            if let Some((label, at)) = self.running.take() {
                // Only an activity that was actually *named* earns a check. One that came and went
                // inside the debounce never appeared on the row, so marking it done would be a
                // check with nothing to refer to — it would flash for an activity the operator
                // never saw start.
                if now.saturating_duration_since(at) >= DEBOUNCE {
                    self.finished = Some((label, now));
                }
            }
            if let Some((label, at)) = &self.finished
                && now.saturating_duration_since(*at) < LINGER
            {
                return format!("{} {DONE}", activity_label(label));
            }
            // The check has been up long enough, or there was nothing to check.
            self.finished = None;
            return THINKING.to_owned();
        }

        // A new activity supersedes a lingering check: the row describes what is happening now, and
        // leaving a stale check beside a running tool would claim both.
        self.finished = None;
        match &self.running {
            Some((label, at)) if label == status => {
                if now.saturating_duration_since(*at) >= DEBOUNCE {
                    activity_label(status)
                } else {
                    // Real, but too new to be worth a name yet.
                    THINKING.to_owned()
                }
            }
            _ => {
                // First sight of it: start the clock and say nothing yet.
                self.running = Some((status.to_owned(), now));
                THINKING.to_owned()
            }
        }
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

    /// A fast activity is never named: it is held back until it has been running long enough to be
    /// worth reading, so a run of quick tools does not strobe names across the row.
    #[test]
    fn a_fast_activity_is_never_named() {
        let mut activity = Activity::new();
        let start = Instant::now();
        assert_eq!(activity.label("Read(a.rs)", start), THINKING);

        // Still within the debounce window: no name yet, and not because the rules forgot it.
        assert_eq!(
            activity.label("Read(a.rs)", start + DEBOUNCE / 2),
            THINKING,
            "half the debounce is not long enough"
        );

        // Past it, the name appears.
        assert_eq!(activity.label("Read(a.rs)", start + DEBOUNCE), "Thinking - Read(a.rs)");
    }

    /// The clock restarts for a *different* activity: a tool that is replaced by another has not
    /// earned its own name yet, even though the previous one had.
    #[test]
    fn the_debounce_restarts_for_each_activity() {
        let mut activity = Activity::new();
        let start = Instant::now();
        assert_eq!(activity.label("Read(a.rs)", start), THINKING);
        assert_eq!(activity.label("Read(a.rs)", start + DEBOUNCE), "Thinking - Read(a.rs)");

        // A second tool arriving after the first has been showing is still new.
        let second = start + DEBOUNCE + Duration::from_millis(10);
        assert_eq!(activity.label("Bash(cargo test)", second), THINKING);
        assert_eq!(
            activity.label("Bash(cargo test)", second + DEBOUNCE),
            "Thinking - Bash(cargo test)"
        );
    }

    /// Take an activity from first sight to named, returning the instant it was named.
    ///
    /// Two calls, because naming is not the first sight of a status: the debounce starts when the
    /// row first sees it, so a test that jumps straight to `start + DEBOUNCE` is really asserting
    /// about a clock that began at that moment. Spelling the sequence out keeps the tests honest
    /// about which call starts the clock.
    fn named(activity: &mut Activity, status: &str, start: Instant) -> Instant {
        assert_eq!(
            activity.label(status, start),
            THINKING,
            "the first sight of an activity must be debounced"
        );
        let shown = start + DEBOUNCE;
        assert_eq!(
            activity.label(status, shown),
            activity_label(status),
            "the name must appear once the debounce has passed"
        );
        shown
    }

    /// A finished activity leaves a check behind rather than vanishing, because the check is the
    /// only signal that it *completed* — while it runs and after it finished, the model is thinking
    /// either way.
    #[test]
    fn a_finished_activity_lingers_with_a_check() {
        let mut activity = Activity::new();
        let start = Instant::now();
        let running = named(&mut activity, "Read(a.rs)", start);

        // The agent reports the marker again: the tool returned.
        assert_eq!(activity.label(THINKING_MARKER, running), "Thinking - Read(a.rs) ✓");
        // And it stays for a moment, so it can actually be seen.
        assert_eq!(
            activity.label(THINKING_MARKER, running + LINGER / 2),
            "Thinking - Read(a.rs) ✓"
        );
        // Then goes, leaving the row as the plain label.
        assert_eq!(
            activity.label(THINKING_MARKER, running + LINGER),
            THINKING,
            "the check must not stay forever"
        );
    }

    /// A tool that finished while the row was still inside its debounce leaves no check: nothing was
    /// ever named, so there is nothing to mark as done — and a check for an unread name would be a
    /// flicker with no referent.
    #[test]
    fn a_tool_that_returns_before_the_debounce_leaves_no_check() {
        let mut activity = Activity::new();
        let start = Instant::now();
        assert_eq!(activity.label("Read(a.rs)", start), THINKING);
        // Gone again before it was ever named.
        assert_eq!(activity.label(THINKING_MARKER, start + DEBOUNCE / 2), THINKING);
    }

    /// A new activity clears a lingering check: the row describes what is happening now, and a
    /// check beside a running tool would claim both had finished.
    #[test]
    fn a_new_activity_clears_the_check() {
        let mut activity = Activity::new();
        let start = Instant::now();
        let running = named(&mut activity, "Read(a.rs)", start);
        assert_eq!(activity.label(THINKING_MARKER, running), "Thinking - Read(a.rs) ✓");

        // The next tool starts while the check is still up; it is new, so it is debounced and the
        // stale check must be gone.
        let next = running + Duration::from_millis(50);
        assert_eq!(
            activity.label("Bash(cargo test)", next),
            THINKING,
            "the check must not survive into the next activity"
        );
        assert_eq!(
            activity.label("Bash(cargo test)", next + DEBOUNCE),
            "Thinking - Bash(cargo test)"
        );
    }

    /// The row starts as plain `Thinking` on a fresh turn, and stays there while the model is being
    /// called — no check for work that has not happened yet.
    #[test]
    fn a_turn_starts_with_nothing_to_check() {
        let mut activity = Activity::new();
        let start = Instant::now();
        assert_eq!(activity.label(THINKING_MARKER, start), THINKING);
        assert_eq!(
            activity.label(THINKING_MARKER, start + Duration::from_secs(5)),
            THINKING
        );
    }
    fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> crate::cost::Tokens {
        crate::cost::Tokens {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    /// With no rate known, the line says so and names the total — and the figures have to add up
    /// against the totals line, which is the bug this exists for.
    ///
    /// The totals line counts only input and output. The unpriced figure counts all four kinds,
    /// because a rate applies to the whole request. On a cached session the cache is most of it, so
    /// the two numbers differed by millions with nothing on screen to explain why — the broken
    /// display read `1,520,678 in - 90,732 out` next to `no price for 8,470,892 tokens`.
    #[test]
    fn the_unpriced_total_says_how_much_of_it_is_cache() {
        let line = cost_line(
            Some("deepseek-flash"),
            &[],
            tokens(1_520_678, 90_732, 6_000_000, 859_482),
            200,
        );
        // The total is all four kinds: 1,520,678 + 90,732 + 6,000,000 + 859,482 = 8,470,892.
        assert!(line.contains("no price for 8,470,892 tokens"), "{line}");
        assert!(line.contains("deepseek-flash"), "{line}");
        // And the cache share is named, so the difference from `in - out` is explicable rather than
        // mysterious. 6,000,000 + 859,482 = 6,859,482.
        assert!(line.contains("of which 6,859,482 cache"), "{line}");
        // The arithmetic the reader needs is checkable from the screen: total - cache = in + out.
        assert_eq!(8_470_892 - 6_859_482, 1_520_678 + 90_732);
    }

    /// With no cache there is nothing to explain, so the note is absent rather than `of which 0
    /// cache` — a note that explains nothing is noise.
    #[test]
    fn no_cache_means_no_cache_note() {
        let line = cost_line(Some("gpt-4"), &[], tokens(100, 20, 0, 0), 200);
        assert!(line.contains("no price for 120 tokens"), "{line}");
        assert!(!line.contains("cache"), "there is no cache to name: {line}");
    }

    /// Priced tokens show the money, and unpriced tokens *beside* it — both facts matter, and
    /// dropping either would hide work that happened.
    #[test]
    fn a_priced_mix_still_reports_the_unpriced_remainder() {
        let amounts = vec![("USD".to_owned(), 1.2345)];
        let line = cost_line(Some("claude-sonnet-4-6"), &amounts, tokens(500, 100, 2000, 0), 200);
        assert!(line.contains("USD"), "{line}");
        assert!(line.contains("1.23"), "{line}");
        // The remainder is reported with its cache share, since 2,000 of those are cache.
        assert!(line.contains("+2,600 tokens unpriced"), "{line}");
        assert!(line.contains("of which 2,000 cache"), "{line}");
    }

    /// Nothing counted at all produces no cost claim, rather than the word "no price" over zero
    /// tokens — which would read as a failure on a session that has simply not run a turn yet.
    #[test]
    fn nothing_counted_says_nothing() {
        let line = cost_line(Some("m"), &[], crate::cost::Tokens::default(), 200);
        assert_eq!(line.trim(), "m");
        assert!(!line.contains("no price"), "{line}");
    }

    /// Every kind of token counts towards the unpriced total, so a session that is *all* cache —
    /// which a cached conversation mostly is — does not report zero.
    #[test]
    fn cache_only_tokens_are_not_reported_as_nothing() {
        let line = cost_line(None, &[], tokens(0, 0, 5_000, 1_000), 200);
        assert!(line.contains("no price for 6,000 tokens"), "{line}");
        assert!(line.contains("of which 6,000 cache"), "{line}");
    }

    /// `cached` is the sum of both cache kinds, which is what the note prints.
    #[test]
    fn the_cache_share_is_both_kinds() {
        assert_eq!(tokens(1, 1, 10, 5).cached(), 15);
        assert_eq!(tokens(1, 1, 0, 0).cached(), 0);
        // And the total is everything, cache included.
        assert_eq!(tokens(1, 1, 10, 5).total(), 17);
    }
}
