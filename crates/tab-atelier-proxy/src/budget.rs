// SPDX-License-Identifier: MPL-2.0
//
// What the completion budget may be, given what the conversation already uses.

/// Room kept free at the top of the context window.
///
/// **Taken off the context side, not the completion side.** The input figure is
/// an estimate — the proxy counts bytes before sending, and the provider counts
/// tokens after receiving — so the place where the estimate can be wrong is the
/// input. Reserving the margin there means the completion keeps whatever the
/// caller asked for, and the cushion absorbs the difference between "3.7 MB" and
/// "921,123 tokens".
///
/// 8,192 tokens is roughly 32 KB of text at the usual four bytes per token: far
/// more than a serialisation-versus-tokenisation discrepancy on a large body,
/// and 0.8% of a million-token window.
pub const CONTEXT_MARGIN: u64 = 8_192;

/// The smallest completion worth asking for.
///
/// Below this the request would technically fit but the answer would be useless,
/// so refusing is more honest than sending it and truncating. A client that
/// hits this needs to compact, and the error says so.
pub const MIN_COMPLETION: u64 = 4_096;

/// What to do with a request's completion budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The request fits as asked. `completion` is what the caller requested.
    Unchanged { completion: u64 },
    /// The request fits only with a smaller completion. `completion` is what is
    /// left after the conversation and the margin.
    Clamped { requested: u64, completion: u64 },
    /// Not even [`MIN_COMPLETION`] fits. The conversation itself is too long,
    /// and only the client can fix that by compacting.
    TooLong { input: u64, usable: u64 },
}

/// Decide the completion budget for one request.
///
/// The arithmetic, in full, because this is the whole fix:
///
/// ```text
///   window           the model's context, e.g. 1,048,576
///   margin           CONTEXT_MARGIN, taken off the CONTEXT side
///   usable input     window - margin
///   left for output  usable input - input
///   completion       min(what was asked, left for output)
/// ```
///
/// The failure this replaces: a request with 921,123 estimated input tokens and
/// `max_tokens: 128000` asked for 1,049,123 tokens against a 1,048,576 window —
/// 547 over — and the provider refused the whole request. Every long session
/// hit it, because the output reservation is 12% of the window and nothing was
/// subtracting it.
#[must_use]
pub const fn fit(window: u64, input: u64, requested: u64) -> Budget {
    let usable = window.saturating_sub(CONTEXT_MARGIN);
    if input >= usable {
        return Budget::TooLong { input, usable };
    }
    let left = usable - input;
    if left < MIN_COMPLETION {
        return Budget::TooLong { input, usable };
    }
    if requested <= left {
        Budget::Unchanged { completion: requested }
    } else {
        Budget::Clamped {
            requested,
            completion: left,
        }
    }
}

/// Estimated input tokens for a request body.
///
/// Four bytes per token, the same approximation `qos::estimate_cost` uses: it is
/// what is knowable before the provider has seen the request. `CONTEXT_MARGIN`
/// exists to absorb how wrong it can be.
#[must_use]
pub const fn estimate_input(body: &[u8]) -> u64 {
    (body.len() as u64) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window every model in the catalogue has today.
    const WINDOW: u64 = 1_048_576;

    /// **The regression test for the reported failure.**
    ///
    /// A session at 921,123 input tokens asking for the model's full 128K output
    /// reserved 1,049,123 against a 1,048,576 window — 547 over — and the
    /// provider refused the entire request. The message the user saw:
    ///
    /// > This model's maximum context length is 1048576 tokens. However, you
    /// > requested 1049123 tokens (921123 in the messages, 128000 in the
    /// > completion).
    ///
    /// With the budget applied the same conversation is served, with the
    /// completion reduced to what actually fits.
    #[test]
    fn the_reported_overflow_is_clamped_instead_of_refused() {
        let budget = fit(WINDOW, 921_123, 128_000);

        let Budget::Clamped { requested, completion } = budget else {
            panic!("expected a clamp, got {budget:?}");
        };
        assert_eq!(requested, 128_000, "what the client asked for is remembered");
        assert_eq!(completion, WINDOW - CONTEXT_MARGIN - 921_123);
        assert!(
            completion > 100_000,
            "still a large completion — the point is to serve the request, not to shrink it away: {completion}",
        );
    }

    /// A request that fits is left alone.
    ///
    /// Clamping unconditionally would quietly reduce every completion to the
    /// sliver left after a long conversation, turning healthy requests into
    /// truncated ones.
    #[test]
    fn a_request_that_fits_is_untouched() {
        assert_eq!(fit(WINDOW, 10_000, 128_000), Budget::Unchanged { completion: 128_000 },);
    }

    /// The margin comes off the input side, which is the side that can be wrong.
    ///
    /// With 40,000 tokens of headroom and a 10,000-token request, the completion
    /// may be 30,000 — not 30,000 minus a second margin. Reserving the cushion
    /// twice would shrink completions for no reason.
    #[test]
    fn the_margin_is_reserved_once_and_on_the_context() {
        let requested = 100_000;
        let input = WINDOW - CONTEXT_MARGIN - 40_000;

        assert_eq!(
            fit(WINDOW, input, requested),
            Budget::Clamped {
                requested,
                completion: 40_000
            },
        );
    }

    /// Exactly at the boundary the request still fits.
    ///
    /// `input + completion == usable` is the largest legal request, and an
    /// off-by-one here would refuse the one request that is precisely full.
    #[test]
    fn a_full_but_legal_request_fits() {
        let input = WINDOW - CONTEXT_MARGIN - MIN_COMPLETION;

        assert_eq!(
            fit(WINDOW, input, MIN_COMPLETION),
            Budget::Unchanged {
                completion: MIN_COMPLETION
            },
        );
    }

    /// One token more and the conversation itself is too long.
    ///
    /// This is the case the client must fix by compacting, so it is reported
    /// rather than clamped to nothing — a completion of zero tokens is not a
    /// request any provider accepts.
    #[test]
    fn a_conversation_with_no_room_left_is_too_long() {
        assert_eq!(
            fit(WINDOW, WINDOW - CONTEXT_MARGIN, 128_000),
            Budget::TooLong {
                input: WINDOW - CONTEXT_MARGIN,
                usable: WINDOW - CONTEXT_MARGIN
            },
        );
        // And one that overflows outright, without wrapping the subtraction.
        assert!(matches!(fit(WINDOW, WINDOW + 5_000, 128_000), Budget::TooLong { .. }));
    }

    /// A body larger than the window does not underflow.
    ///
    /// `window - input` would wrap on a body past the window, producing a
    /// colossal "completion" that the provider then rejects for the original
    /// reason — the failure this replaces, with extra steps.
    #[test]
    fn a_body_past_the_window_saturates() {
        let budget = fit(WINDOW, WINDOW * 2, 128_000);
        assert!(matches!(budget, Budget::TooLong { .. }), "got {budget:?}");
    }

    /// The estimate is the documented four-bytes-per-token rule.
    #[test]
    fn the_input_estimate_is_bytes_over_four() {
        assert_eq!(estimate_input(&[0u8; 4_000]), 1_000);
        assert_eq!(estimate_input(&[]), 0);
    }
}
