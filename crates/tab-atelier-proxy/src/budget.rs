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
/// Four bytes per token, the same approximation `qos::estimate_cost` uses — but
/// **only for text**. Base64 payloads are excluded and charged a flat per-image
/// cost, because four-bytes-per-token is meaningless for them: a 1.5 MB
/// screenshot is ~2 MB of base64, which this rule would score at half a million
/// tokens for something the provider counts in the low thousands. Three
/// screenshots in a session then read as "past the window", and a proxy that
/// believes that refuses a request that would have succeeded.
///
/// `raw_len` is the serialised body length, which already includes the base64.
/// The `data` strings are subtracted from it and [`IMAGE_TOKENS`] is added back
/// per image, so the text around them is still measured honestly.
#[must_use]
pub fn estimate_input(body: &serde_json::Value, raw_len: usize) -> u64 {
    let mut media_bytes = 0u64;
    let mut media_blocks = 0u64;
    collect_media(body, &mut media_bytes, &mut media_blocks);

    let text_bytes = (raw_len as u64).saturating_sub(media_bytes);
    // The `+ 4` on each image is the four-bytes-per-token conversion applied to
    // the nominal cost, so one arithmetic runs through the whole figure.
    (text_bytes + media_blocks * IMAGE_TOKENS * 4) / 4
}

/// What one image is assumed to cost, in tokens.
///
/// Anthropic's documented figure for a full-size image is roughly 1,600 tokens
/// (about 1,600 px on the long edge); smaller ones cost less. This is an upper
/// bound on purpose — the number only ever *lowers* the completion reservation,
/// and over-counting a little costs a slightly smaller completion, while
/// under-counting means the provider refuses the request.
pub const IMAGE_TOKENS: u64 = 1_600;

/// Sum the bytes of base64 media and count the blocks carrying it.
///
/// Walks the whole body rather than only `messages`, because a tool result can
/// carry an image nested inside it and the top-level shape differs between the
/// two wires.
fn collect_media(value: &serde_json::Value, bytes: &mut u64, blocks: &mut u64) {
    match value {
        serde_json::Value::Object(map) => {
            let is_media = matches!(
                map.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "document")
            );
            if is_media {
                // The payload sits at `source.data` on the Anthropic wire and at
                // `source.data` on the OpenAI one too, once converted.
                if let Some(data) = map
                    .get("source")
                    .and_then(|s| s.get("data"))
                    .and_then(serde_json::Value::as_str)
                {
                    *bytes = bytes.saturating_add(data.len() as u64);
                    *blocks = blocks.saturating_add(1);
                }
            }
            for v in map.values() {
                collect_media(v, bytes, blocks);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_media(v, bytes, blocks);
            }
        }
        _ => {}
    }
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

    /// The estimate is the documented four-bytes-per-token rule, for text.
    #[test]
    fn the_input_estimate_is_bytes_over_four_for_text() {
        let body = serde_json::json!({ "messages": [] });
        let raw = serde_json::to_vec(&body).expect("serialises").len();

        assert_eq!(estimate_input(&body, raw), (raw as u64) / 4);
        assert_eq!(estimate_input(&serde_json::Value::Null, 0), 0);
    }

    /// **The regression test for the hang.**
    ///
    /// A screenshot is base64 in the body: a 1.5 MB PNG is about 2 MB of
    /// characters, which four-bytes-per-token scores at ~500,000 tokens for
    /// something the provider counts as ~1,600. Three of them read as "past a
    /// million-token window", and the proxy this estimate drives refused the
    /// request — so the client compacted, resent the same images, was refused
    /// again, and the session looped forever instead of failing once.
    #[test]
    fn an_image_is_charged_a_nominal_cost_not_its_base64_size() {
        // ~2 MB of base64, the size of a real screenshot block.
        let payload = "A".repeat(2_000_000);
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": payload },
                }],
            }],
        });
        let raw = serde_json::to_vec(&body).expect("serialises").len();

        let estimate = estimate_input(&body, raw);

        assert!(
            estimate < IMAGE_TOKENS * 2,
            "one image is a nominal cost, not half a million tokens: {estimate}",
        );
        assert!(estimate >= IMAGE_TOKENS, "but it is not free either: {estimate}");
    }

    /// Text beside an image is still measured, so the exclusion is not a blanket
    /// discount that would let a genuinely long prompt through unmeasured.
    #[test]
    fn text_beside_an_image_is_still_counted() {
        let payload = "A".repeat(100_000);
        let long_text = "word ".repeat(20_000); // ~100 KB of text
        let body = serde_json::json!({
            "messages": [{
                "content": [
                    { "type": "text", "text": long_text },
                    { "type": "image", "source": { "data": payload } },
                ],
            }],
        });
        let raw = serde_json::to_vec(&body).expect("serialises").len();

        let estimate = estimate_input(&body, raw);

        assert!(estimate > 20_000, "100 KB of text is measured normally: {estimate}");
    }
}
