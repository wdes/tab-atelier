// SPDX-License-Identifier: MPL-2.0

//! Retrying a rate-limited or temporarily-failed turn.
//!
//! Until now a 429 or a 5xx ended the turn outright: `AgentError::Api` was
//! returned, the REPL printed it, and whatever the user had asked for was
//! half-done. That is the worst possible response to a rate limit, because a
//! rate limit is a statement about *timing* rather than about the request — the
//! identical call succeeds a few seconds later, and every other client in this
//! workspace (the proxy's `qos`, Claude Code itself) already waits.
//!
//! # What is retried, and what is not
//!
//! Retrying is only safe when the request would have succeeded unchanged, so
//! the decision is made by status class rather than by a message:
//!
//! - **429** and **529** — throttled and overloaded. Both mean "later".
//! - **5xx** other than 501 — a server-side failure that a retry may clear. 501
//!   is excluded: "not implemented" is a statement about the request, and
//!   repeating it wastes the user's time.
//! - **408** — a request timeout, which is the same shape as a throttling.
//!
//! Everything else is returned to the caller immediately. In particular a 400
//! or a 401 must never be retried: they are deterministic, and five attempts at
//! a malformed request is five times the latency for the same answer.
//!
//! `Retry-After` is honoured when present, in both of its forms — a number of
//! seconds and an HTTP date — because the proxy emits the former and a
//! provider may emit the latter. It is clamped: a server that asks for an hour
//! should not silently park the agent for an hour, and a server that asks for
//! zero should not turn the loop into a spin.

use std::time::Duration;

/// How many times a retryable failure is tried again.
///
/// Four attempts, so three retries. Enough to ride out a burst or a rolling
/// restart, few enough that a genuinely unavailable provider surfaces as an
/// error in under a minute rather than hanging the agent.
pub const MAX_ATTEMPTS: u32 = 4;

/// The first delay, doubling each attempt.
const BASE_DELAY: Duration = Duration::from_millis(500);

/// The longest a single wait may be, however long the server asks for.
///
/// A `Retry-After` of an hour is a real thing to receive and not a thing to
/// obey: the user is sitting in front of this, and the honest response to "come
/// back in an hour" is to say so rather than to appear to work for an hour.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// Whether a status is worth trying again.
///
/// See the module note for why each class is here. Kept as a pure function of
/// the code so the policy can be read and tested without a server.
#[must_use]
pub const fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// How long to wait before attempt `attempt`, given the server's own hint.
///
/// `attempt` is 1-based: the wait before the second attempt is `attempt = 2`.
/// The hint wins when it is present, because the server knows when its window
/// reopens and a guess is worse than being told.
#[must_use]
pub fn delay_for(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let backoff = BASE_DELAY.saturating_mul(1u32 << attempt.saturating_sub(1).min(5));
    retry_after.unwrap_or(backoff).min(MAX_DELAY)
}

/// Parse a `Retry-After` header, in either of its two forms.
///
/// Returns `None` for anything unparseable, which then falls back to the
/// exponential delay — a malformed hint is not a reason to retry immediately.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // The date form, per RFC 9110. Its IMF-fixdate is RFC 2822's format, which
    // is what this parses — the HTTP spec narrowed RFC 2822 down to one of its
    // forms rather than inventing a new one, so no second grammar is needed.
    let when = jiff::fmt::rfc2822::parse(trimmed).ok()?;
    let now = jiff::Timestamp::now();
    // A date in the past means "now" rather than a negative wait.
    Some(Duration::from_secs(
        (when.timestamp().as_second() - now.as_second()).max(0).unsigned_abs(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_and_server_faults_are_retried() {
        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable(status), "{status} should be retried");
        }
    }

    /// The important half: a deterministic failure must not be repeated. Five
    /// attempts at a malformed request is five times the latency and the same
    /// answer, and the user cannot tell it apart from the agent being slow.
    #[test]
    fn deterministic_failures_are_not_retried() {
        for status in [400, 401, 403, 404, 413, 422, 501] {
            assert!(!is_retryable(status), "{status} should not be retried");
        }
        // 501 is the interesting one: it is a 5xx, but "not implemented" is
        // about the request rather than the server's mood.
        assert!(!is_retryable(501));
    }

    #[test]
    fn the_delay_grows_and_is_capped() {
        let first = delay_for(2, None);
        let second = delay_for(3, None);
        assert!(first < second, "the backoff must grow: {first:?} then {second:?}");
        assert!(delay_for(20, None) <= MAX_DELAY, "the backoff must be capped");
    }

    /// A server that knows when its window reopens is believed, up to the cap.
    #[test]
    fn the_servers_hint_wins_until_it_is_unreasonable() {
        assert_eq!(delay_for(2, Some(Duration::from_secs(7))), Duration::from_secs(7));
        assert_eq!(
            delay_for(2, Some(Duration::from_hours(1))),
            MAX_DELAY,
            "an hour is not something to wait out silently"
        );
        // Asking for no wait must not become a spin: the hint is taken as
        // given, so this is the caller's reminder that a zero is possible.
        assert_eq!(delay_for(2, Some(Duration::ZERO)), Duration::ZERO);
    }

    #[test]
    fn a_seconds_hint_parses() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("  0 "), Some(Duration::ZERO));
    }

    /// The date form, and the reason it is worth supporting: a provider that
    /// sends `Retry-After: Sun, 06 Nov 1994 08:49:37 GMT` is not unusual, and
    /// treating it as unparseable would fall back to a shorter guess.
    ///
    /// The date is *generated* rather than written out, because the parser
    /// checks the weekday against the date and hand-written examples get that
    /// wrong. The HTTP spec's own example is correct because 1994-11-06 really
    /// was a Sunday; an invented "Sun, 06 Nov 2098" is not a Sunday, and that
    /// string fails to parse for a reason with nothing to do with this code.
    #[test]
    fn a_date_hint_parses() {
        let soon = jiff::Timestamp::now()
            .checked_add(Duration::from_mins(10))
            .expect("now plus ten minutes")
            .to_zoned(jiff::tz::TimeZone::UTC);
        let printed = jiff::fmt::rfc2822::to_string(&soon).expect("prints as an HTTP date");
        let parsed = parse_retry_after(&printed).expect("and parses back");
        // Within a few seconds of the ten minutes asked for: the round trip
        // loses sub-second precision and nothing more.
        assert!(
            parsed >= Duration::from_secs(595) && parsed <= Duration::from_secs(605),
            "expected about 600s, got {parsed:?} from {printed:?}"
        );
        // The parse does not clamp — ten minutes is far past `MAX_DELAY`, and
        // saying otherwise here was wrong. Clamping is `delay_for`'s job, and
        // keeping it in one place is what stops two rules disagreeing about
        // one number.
        assert!(parsed > MAX_DELAY, "the raw hint is not clamped by the parse");
        assert_eq!(delay_for(2, Some(parsed)), MAX_DELAY, "but the wait taken is");
    }

    /// The past is "now", not a negative duration — a clock skew between here
    /// and the server must not produce a nonsense wait. Uses the HTTP spec's
    /// own example, which is a real date with a correct weekday.
    #[test]
    fn a_date_already_past_is_no_wait() {
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"), Some(Duration::ZERO));
    }

    #[test]
    fn nonsense_falls_back_rather_than_retrying_immediately() {
        for value in ["", "soon", "-5", "1.5", "Wed, 32 Foo 2026 99:99:99 GMT"] {
            assert_eq!(parse_retry_after(value), None, "{value:?} should not parse");
        }
    }
}
