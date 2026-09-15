// SPDX-License-Identifier: MPL-2.0

//! Usage, rolled up over a window.
//!
//! These are named structs rather than hand-built `json!` values because the
//! `OpenAPI` document is generated from them: a field that exists only inside a
//! macro would be missing from the spec the UI is written against.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::provider;
use crate::usage;

/// A window's tokens, flattened beside its call counts.
#[derive(Serialize)]
pub(crate) struct TokenResource {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The rest of the upstream's usage block. Carried through rather than
    /// summed away: the cache-write TTL split and the server-tool counts each
    /// explain part of a bill the four token figures do not.
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub web_search: u64,
    pub web_fetch: u64,
    pub service_tier: Option<usage::Tier>,
    pub total: u64,
}

impl From<&usage::Tokens> for TokenResource {
    fn from(t: &usage::Tokens) -> Self {
        Self {
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            cache_write: t.cache_write,
            cache_write_5m: t.cache_write_5m,
            cache_write_1h: t.cache_write_1h,
            web_search: t.web_search,
            web_fetch: t.web_fetch,
            service_tier: t.service_tier,
            total: t.total(),
        }
    }
}

/// What a batch of tokens cost, in micro-USD.
///
/// Split the way the chart is drawn. The two panels are priced at different
/// rates — 50:1 between a cache read and a miss on Flash — so one combined
/// total could not be put back onto them, and a figure that cannot be drawn
/// where it belongs is a figure nobody can check.
///
/// Micro-USD rather than a float: this is a sum of many small per-request
/// costs, and binary floating point accumulates the error in the direction
/// nobody looks. Shown as dollars only at the last moment, by the UI.
#[derive(Serialize)]
pub(crate) struct CostResource {
    /// Cache reads and misses, each at its own published rate, plus cache
    /// writes at the miss rate — a write is a first read that pays full price.
    pub in_micro: i128,
    /// Everything the model generated.
    pub out_micro: i128,
}

impl CostResource {
    /// Price one batch of tokens, or `None` when the hop publishes no rates.
    ///
    /// `None` rather than zeroes: a flat-rate subscription costs real money
    /// that these token counts do not measure, and rendering that as `$0.00`
    /// would be a different claim from "unpriced".
    ///
    /// Takes an already-resolved `price` rather than a rate and an instant, so
    /// that the peak question is answered at the call site where it can be
    /// explained. The two callers answer it differently, and deliberately — see
    /// `WindowResource::of` and `BucketResource::of`.
    fn of(t: &usage::Tokens, price: Option<provider::Price>) -> Option<Self> {
        let price = price?;
        let cache_read = t.cache_read.saturating_add(t.cache_write);
        let (in_micro, out_micro) = price.split_micro(cache_read, t.input, t.output);
        Some(Self { in_micro, out_micro })
    }
}

/// Calls and tokens over one span.
#[derive(Serialize)]
pub(crate) struct WindowResource {
    /// Calls served, including the ones that failed — a failure is still a
    /// call against the rate limit.
    pub calls: u64,
    /// Calls the upstream refused, kept apart so a spike of failures cannot
    /// read as a spike of usage.
    pub errors: u64,
    pub tokens: TokenResource,
    /// What `tokens` cost, or `None` when the hop is unpriced.
    pub cost: Option<CostResource>,
}

impl WindowResource {
    fn of(u: &usage::Store, id: &str, window: usage::Window, now: u64, rate: Option<&provider::Rate>) -> Self {
        let span = window.span(now);
        let (calls, errors, tokens) = u.totals(id, span);
        Self {
            calls,
            errors,
            tokens: TokenResource::from(&tokens),
            // Base rates, *not* `rate.at(span.last)`. A window total priced at
            // its final instant would make the same seven days of history read
            // double or half depending on what time of day you happened to
            // refresh — a money figure that moves while the data does not is
            // worse than one that is uniformly off-peak. The chart, which knows
            // each hour, is the surface that charges peak exactly.
            cost: CostResource::of(&tokens, rate.map(|r| r.price)),
        }
    }
}

/// One hour of the dense series.
///
/// A narrower shape than [`TokenResource`] on purpose: the chart plots these
/// four series and nothing else, and the cache-write TTL split would be four
/// more legend entries for one bar.
#[derive(Serialize)]
pub(crate) struct BucketResource {
    pub hour: u64,
    pub calls: u64,
    pub errors: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// This hour's cost, split as the chart draws it.
    ///
    /// `None` on an unpriced hop, never `0`: the money unit hides itself rather
    /// than plotting an hour that looks free. `Option` rather than a flag plus
    /// zeroes, so a caller cannot read the number without having handled the
    /// absent case.
    pub cost_in_micro: Option<i128>,
    pub cost_out_micro: Option<i128>,
}

impl BucketResource {
    /// `hour` doubles as the pricing instant, so a peak hour is charged at peak
    /// without the chart having to know the schedule.
    fn of(b: &usage::Bucket, rate: Option<&provider::Rate>) -> Self {
        // `b.hour` is the pricing instant, so an hour billed at peak is priced
        // at peak and the hours either side of a boundary are not.
        let cost = CostResource::of(&b.tokens, rate.map(|r| r.at(b.hour)));
        Self {
            hour: b.hour,
            calls: b.calls,
            errors: b.errors,
            input: b.tokens.input,
            output: b.tokens.output,
            cache_read: b.tokens.cache_read,
            cache_write: b.tokens.cache_write,
            cost_in_micro: cost.as_ref().map(|c| c.in_micro),
            cost_out_micro: cost.as_ref().map(|c| c.out_micro),
        }
    }
}

/// The span the numbers cover, as timestamps rather than a pidgin of units.
///
/// Both clients format from these and neither re-derives them, so the range the
/// charts draw is the range the server measured.
#[derive(Serialize)]
pub(crate) struct SpanResource {
    pub start: String,
    pub end: String,
    pub hours: u64,
}

/// What `/api/usage` and `/me/usage` return.
#[derive(Serialize)]
pub(crate) struct UsageResource {
    pub window: SpanResource,
    /// The two fixed windows stay: they are what the account table and the
    /// "average tokens per call" column read, and they must mean the same thing
    /// whatever the graph is currently zoomed to. `window` carries the
    /// caller's requested span separately.
    pub all_time: WindowResource,
    pub last_24h: WindowResource,
    pub last_7d: WindowResource,
    pub by_model: BTreeMap<String, TokenResource>,
    /// Dense hourly buckets, oldest first, ending at the current hour.
    pub series_hourly: Vec<BucketResource>,
    pub retained_hours: u64,
    /// The model whose published rates the money figures used, or `None` when
    /// this account's hop has no published rate at all.
    ///
    /// The client decides whether to offer the money unit off this field, and
    /// shows the name beside the figure. It is a model rather than a provider
    /// because the rates are per model, and it names one model for the whole
    /// account because a usage bucket records tokens, not which model among
    /// several earned them. Exact for the traffic here — each account reaches
    /// one model — and an approximation worth stating if that stops holding.
    pub cost_model: Option<String>,
}

/// Render one account's usage.
///
/// `now` is passed rather than read here, so the requested window and the two
/// fixed ones are resolved from one reading of the clock. `rate` is explicit
/// for the same reason: the caller holds the registry lock, and resolving it
/// here would mean taking one.
impl UsageResource {
    #[must_use]
    pub(crate) fn of(u: &usage::Store, id: &str, span: usage::Span, now: u64, rate: Option<&provider::Rate>) -> Self {
        Self {
            window: SpanResource {
                start: crate::now_rfc3339_at(span.first),
                end: crate::now_rfc3339_at(span.last),
                hours: span.hours(),
            },
            all_time: WindowResource::of(u, id, usage::Window::All, now, rate),
            last_24h: WindowResource::of(u, id, usage::Window::Hours(24), now, rate),
            last_7d: WindowResource::of(u, id, usage::Window::Hours(24 * 7), now, rate),
            by_model: u
                .for_account(id)
                .map(|a| {
                    a.by_model
                        .iter()
                        .map(|(m, t)| (m.clone(), TokenResource::from(t)))
                        .collect()
                })
                .unwrap_or_default(),
            series_hourly: u.series(id, span).iter().map(|b| BucketResource::of(b, rate)).collect(),
            retained_hours: usage::RETAIN_HOURS,
            cost_model: rate.map(|r| r.model.clone()),
        }
    }
}

/// `?window=` off a query string, if present and sane.
///
/// An unrecognised token falls back to the default rather than 400ing: this is
/// a GET the dashboard re-issues on a timer, and a stale bookmark naming a
/// window that was removed should show a graph, not an error page.
pub(crate) fn path_window(query: &str) -> Option<usage::Window> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("window="))
        .and_then(usage::Window::parse)
}

/// One account's usage in the installation-wide report.
///
/// The account is flattened in beside the numbers rather than nested, so a
/// dashboard row reads `row.first_name` and `row.last_24h` at the same level.
#[derive(Serialize)]
pub(crate) struct UserUsageResource {
    pub user: crate::http::resources::account::AccountResource,
    #[serde(flatten)]
    pub usage: UsageResource,
}

/// Every account's usage, for the operator view.
#[derive(Serialize)]
pub(crate) struct UsageReportResource {
    /// The window asked for, as given. Kept as the caller's word for it
    /// because "24h" and "7d" are not the same string as their hour counts.
    pub window: String,
    pub hours: u64,
    pub window_start: String,
    pub window_end: String,
    pub users: Vec<UserUsageResource>,
}

/// The caller's own identity, beside their usage.
#[derive(Serialize)]
pub(crate) struct AccountSummary {
    pub name: String,
    pub email: String,
}

/// What `/me/usage` returns: someone's numbers, addressed by their own key.
///
/// The account is flattened in beside the numbers for the same reason as in
/// [`UserUsageResource`]: the caller already knows who they are, and a nested
/// object would only make the agent reading it unwrap one more level.
#[derive(Serialize)]
pub(crate) struct MeUsageResource {
    pub account: AccountSummary,
    #[serde(flatten)]
    pub usage: UsageResource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Span, Tokens, Window, now_secs};

    /// A usage store of this test's own.
    ///
    /// Named after the test, because the store writes to disk and two tests
    /// sharing a root would see each other's calls.
    fn store(name: &str) -> usage::Store {
        let root = std::env::temp_dir()
            .join("tab-atelier-usage-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        usage::Store::load(&root)
    }

    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input,
            output,
            ..Tokens::default()
        }
    }

    /// The span a 24-hour request resolves to, and the moment it was resolved at.
    fn day() -> (Span, u64) {
        let now = now_secs();
        (Window::parse("24h").expect("a known window").span(now), now)
    }

    // ── the query string ────────────────────────────────────────────────────

    #[test]
    fn a_window_is_found_among_other_parameters() {
        // The query arrives as one string, so the reader has to pick its
        // parameter out of it rather than assume it is the only one.
        assert!(path_window("window=7d").is_some());
        assert!(path_window("a=1&window=7d&b=2").is_some());
        assert!(path_window("window=7d&a=1").is_some());
    }

    #[test]
    fn a_missing_window_is_absent_rather_than_defaulted() {
        // `None` is what lets the caller pick its own fallback; returning one
        // here would make the caller's default unreachable.
        assert!(path_window("").is_none());
        assert!(path_window("a=1").is_none());
        assert!(path_window("other=7d").is_none());
    }

    #[test]
    fn a_window_that_cannot_be_read_is_absent_not_an_error() {
        // A typo in a query string must not be able to fail a request, so an
        // unreadable token is indistinguishable from a missing one.
        assert!(path_window("window=nonsense").is_none());
        assert!(path_window("window=").is_none());
        assert!(path_window("window=7d").is_some());
    }

    // ── tokens ──────────────────────────────────────────────────────────────

    #[test]
    fn the_reported_total_is_the_sum_of_the_four_counted_kinds() {
        // `total` is the first number on the dashboard. Server-tool calls are
        // counted separately and are not tokens spent on the model, so a total
        // that included them would overstate the bill.
        let t = Tokens {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write: 40,
            web_search: 500,
            ..Tokens::default()
        };
        let r = TokenResource::from(&t);
        assert_eq!(r.total, 100);
        assert_eq!(r.input, 10);
        assert_eq!(r.output, 20);
        assert_eq!(r.cache_read, 30);
        assert_eq!(r.cache_write, 40);
        assert_eq!(r.web_search, 500, "carried through, but not summed in");
    }

    #[test]
    fn the_cache_write_tiers_are_reported_because_they_are_priced_apart() {
        // A five-minute and a one-hour cache write cost different amounts, so
        // collapsing them makes the number unauditable.
        let t = Tokens {
            cache_write_5m: 7,
            cache_write_1h: 11,
            ..Tokens::default()
        };
        let r = TokenResource::from(&t);
        assert_eq!(r.cache_write_5m, 7);
        assert_eq!(r.cache_write_1h, 11);
    }

    #[test]
    fn a_tier_that_was_not_reported_is_absent_rather_than_standard() {
        // "The upstream did not say" and "the upstream said standard" lead to
        // different conclusions about a bill, so they are different values.
        let r = TokenResource::from(&Tokens::default());
        assert!(r.service_tier.is_none());
    }

    // ── an account's numbers ────────────────────────────────────────────────

    #[test]
    fn an_account_nobody_recorded_reports_zeroes_rather_than_being_missing() {
        // The table draws a row per account, so an absent row breaks the page
        // where a zero is the truth.
        let u = store("empty");
        let (span, now) = day();
        let r = UsageResource::of(&u, "nobody", span, now, None);

        assert_eq!(r.last_24h.calls, 0);
        assert_eq!(r.last_24h.tokens.total, 0);
        assert_eq!(r.all_time.calls, 0);
        assert!(r.by_model.is_empty());
        assert_eq!(r.retained_hours, usage::RETAIN_HOURS);
    }

    #[test]
    fn a_recorded_call_is_counted_and_its_tokens_added_up() {
        let mut u = store("one-call");
        u.record("acct", Some("claude-sonnet-4"), tokens(10, 20), true);

        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);

        assert_eq!(r.last_24h.calls, 1);
        assert_eq!(r.last_24h.tokens.input, 10);
        assert_eq!(r.last_24h.tokens.output, 20);
        assert_eq!(r.last_24h.tokens.total, 30);
    }

    #[test]
    fn a_refused_call_counts_as_a_call_and_spends_nothing() {
        // Deliberately apart: a spike of failures must not read as a spike of
        // spending, and a failure the client saw must not vanish from the
        // count.
        let mut u = store("errors");
        u.record("acct", Some("m"), tokens(0, 0), false);

        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);

        assert_eq!(r.last_24h.calls, 1);
        assert_eq!(r.last_24h.errors, 1);
        assert_eq!(r.last_24h.tokens.total, 0);
    }

    #[test]
    fn usage_is_attributed_to_the_model_that_was_billed() {
        // The fallback path rewrites the model under pressure, so what the
        // caller asked for is not always what was paid for. Splitting by model
        // is the only place that difference is visible.
        let mut u = store("by-model");
        u.record("acct", Some("claude-opus-4"), tokens(1, 2), true);
        u.record("acct", Some("claude-haiku-4"), tokens(3, 4), true);

        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);

        assert!(r.by_model.contains_key("claude-opus-4"), "{:?}", r.by_model.keys());
        assert!(r.by_model.contains_key("claude-haiku-4"));
        assert_eq!(r.by_model["claude-opus-4"].input, 1);
        assert_eq!(r.by_model["claude-haiku-4"].input, 3);
    }

    #[test]
    fn a_call_with_no_model_is_still_counted() {
        // A request refused before routing has no model, and dropping it would
        // understate how many calls the account made.
        let mut u = store("no-model");
        u.record("acct", None, tokens(5, 5), true);

        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);
        assert_eq!(r.last_24h.tokens.total, 10);
    }

    #[test]
    fn one_another_accounts_calls_are_not_counted() {
        // The figures are per account; a total that leaked across them would
        // make every row the installation's sum.
        let mut u = store("isolation");
        u.record("mine", Some("m"), tokens(10, 0), true);
        u.record("theirs", Some("m"), tokens(99, 0), true);

        let (span, now) = day();
        let r = UsageResource::of(&u, "mine", span, now, None);
        assert_eq!(r.last_24h.tokens.total, 10);
    }

    #[test]
    fn the_three_fixed_spans_are_reported_together() {
        // The panel shows all time, 24h and 7d at once, so all three have to be
        // in one response rather than fetched a window at a time.
        let mut u = store("fixed-spans");
        u.record("acct", Some("m"), tokens(9, 1), true);

        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);

        assert_eq!(r.last_24h.calls, 1);
        assert_eq!(r.last_7d.calls, 1, "a day is inside a week");
        assert_eq!(r.all_time.calls, 1, "and inside everything");
        assert_eq!(r.last_7d.tokens.total, 10);
    }

    // ── the span and the series ─────────────────────────────────────────────

    #[test]
    fn the_span_is_reported_as_the_timestamps_it_was_given() {
        // The chart draws its axis from these, so they are formatted once here
        // rather than re-derived by each client.
        let u = store("span");
        let now = 1_700_000_000;
        let span = Span {
            first: now - 3600,
            last: now,
        };
        let r = UsageResource::of(&u, "acct", span, now, None);

        assert_eq!(r.window.start, crate::now_rfc3339_at(span.first));
        assert_eq!(r.window.end, crate::now_rfc3339_at(span.last));
        assert_eq!(r.window.hours, span.hours());
    }

    #[test]
    fn the_series_has_one_point_per_hour_of_the_span_not_per_hour_with_traffic() {
        // The chart plots this against a fixed axis, so a series that shrank to
        // the hours with traffic would misalign every point after a gap.
        let u = store("series");
        let now = now_secs();
        let span = Span {
            first: now - 6 * 3600,
            last: now,
        };
        let r = UsageResource::of(&u, "acct", span, now, None);
        assert_eq!(
            r.series_hourly.len(),
            7,
            "seven hours inclusive of both ends, with no traffic at all"
        );
    }

    #[test]
    fn what_is_held_is_reported_against_the_retention_limit() {
        // The UI warns when the oldest data is about to fall off, and it cannot
        // compute that without both numbers.
        let u = store("retention");
        let (span, now) = day();
        let r = UsageResource::of(&u, "acct", span, now, None);
        assert_eq!(r.retained_hours, usage::RETAIN_HOURS);
    }

    // ── the shapes the clients generate types from ──────────────────────────

    #[test]
    fn a_bucket_reports_its_hour_and_its_split() {
        // One bar per bucket, so the hour and the counts travel together.
        let bucket = usage::Bucket {
            hour: 1_700_000_000,
            calls: 3,
            errors: 1,
            tokens: tokens(6, 9),
            by_model: std::collections::BTreeMap::new(),
        };
        let r = BucketResource::of(&bucket, None);
        assert_eq!(r.hour, 1_700_000_000);
        assert_eq!(r.calls, 3);
        assert_eq!(r.errors, 1);
        assert_eq!(r.input, 6);
        assert_eq!(r.output, 9);
        // No rate was supplied, so the hour is unpriced rather than free. The
        // absence is the signal — `0` would be a different claim, and one the
        // money unit would happily plot.
        assert!(r.cost_in_micro.is_none());
        assert!(r.cost_out_micro.is_none());
    }

    #[test]
    fn a_priced_bucket_splits_by_what_each_class_costs() {
        // The two halves must not come out equal for a batch that is not equal:
        // an implementation that priced everything at one rate would pass a
        // test written with symmetric tokens and fail this one.
        let bucket = usage::Bucket {
            hour: 1_700_000_000,
            calls: 1,
            errors: 0,
            tokens: usage::Tokens {
                input: 1_000_000,
                output: 1_000_000,
                ..tokens(0, 0)
            },
            by_model: std::collections::BTreeMap::new(),
        };
        let rate = flat_rate(3_000, 150_000, 600_000);
        let r = BucketResource::of(&bucket, Some(&rate));
        // Off-peak: 1M misses at $0.15 and 1M out at $0.60, in micro-USD.
        assert_eq!(r.cost_in_micro, Some(150_000));
        assert_eq!(r.cost_out_micro, Some(600_000));
    }

    #[test]
    fn a_peak_hour_is_priced_at_peak() {
        // The provider's real schedule, so the test moves if the schedule does:
        // 200% Mon-Fri, 01:00-04:00 and 06:00-10:00 UTC. All of these are
        // Monday 2026-09-14, so the weekday arm is exercised rather than
        // assumed — and 10:00 pins the half-open boundary, which is the
        // off-by-one an implementation is most likely to ship.
        let rate = rate_with_peak();
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 5, 0),
            Some(150_000),
            "Mon 05:00Z is off-peak"
        );
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 6, 0),
            Some(300_000),
            "Mon 06:00Z opens the peak"
        );
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 7, 0),
            Some(300_000),
            "Mon 07:00Z is doubled"
        );
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 9, 59),
            Some(300_000),
            "Mon 09:59Z is the last doubled minute"
        );
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 10, 0),
            Some(150_000),
            "Mon 10:00Z is off-peak again"
        );
    }

    #[test]
    fn a_weekend_hour_is_never_peak() {
        // The same clock hour, a day earlier. If the weekday arm were dropped
        // from the window these two would match, so this is what says the
        // window is "weekday" and not merely "these hours".
        let rate = rate_with_peak();
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 14, 7, 0),
            Some(300_000),
            "Mon 2026-09-14 07:00Z"
        );
        assert_eq!(
            cost_at_utc(&rate, 2026, 9, 13, 7, 0),
            Some(150_000),
            "Sun 2026-09-13 07:00Z, same clock hour"
        );
    }

    /// What 1M miss-priced tokens at the given UTC instant cost, in micro-USD.
    ///
    /// Addressed by calendar date rather than by epoch second on purpose: a
    /// hand-computed epoch is unreviewable, and one that is silently a day out
    /// still passes unless the reader re-derives it.
    fn cost_at_utc(rate: &provider::Rate, y: i16, m: i8, d: i8, h: i8, min: i8) -> Option<i128> {
        let hour = jiff::civil::date(y, m, d)
            .at(h, min, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .expect("UTC resolves every civil time")
            .timestamp()
            .as_second();
        let bucket = usage::Bucket {
            hour: u64::try_from(hour).expect("after the epoch"),
            calls: 1,
            errors: 0,
            tokens: usage::Tokens {
                input: 1_000_000,
                ..tokens(0, 0)
            },
            by_model: std::collections::BTreeMap::new(),
        };
        BucketResource::of(&bucket, Some(rate)).cost_in_micro
    }

    /// A rate with the given published prices and no peak schedule.
    fn flat_rate(cache_hit: u32, input: u32, output: u32) -> provider::Rate {
        provider::Rate {
            price: provider::Price {
                cache_hit,
                input,
                output,
            },
            peak: None,
            model: "test-model".to_owned(),
        }
    }

    /// `DeepSeek` Flash's off-peak rates with its real peak windows.
    fn rate_with_peak() -> provider::Rate {
        provider::Rate {
            peak: Some(provider::Peak {
                multiplier_percent: 200,
                windows: vec![
                    provider::PeakWindow {
                        weekdays: vec![1, 2, 3, 4, 5],
                        start_hour: 1,
                        end_hour: 4,
                    },
                    provider::PeakWindow {
                        weekdays: vec![1, 2, 3, 4, 5],
                        start_hour: 6,
                        end_hour: 10,
                    },
                ],
            }),
            ..flat_rate(3_000, 150_000, 600_000)
        }
    }

    #[test]
    fn a_window_reports_its_calls_beside_its_tokens() {
        let w = WindowResource {
            calls: 2,
            errors: 0,
            tokens: TokenResource::from(&tokens(1, 2)),
            cost: None,
        };
        assert_eq!(w.calls, 2);
        assert_eq!(w.tokens.total, 3);
    }

    #[test]
    fn an_account_row_nests_its_identity_under_one_key() {
        // The row is rendered from a single object, so the account travels with
        // the totals instead of needing a second lookup by id.
        let row_dir = std::env::temp_dir()
            .join("tab-atelier-usage-tests")
            .join(format!("row-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&row_dir);
        let mut users = crate::users::Store::load(row_dir.join("users.json")).expect("a fresh store");
        users.add("Ada", "Lovelace", "row@example.com").expect("add");
        let account = users.find("row@example.com").cloned().expect("added");

        let u = store("row");
        let (span, now) = day();
        let usage = UsageResource::of(&u, "acct", span, now, None);
        let row = UserUsageResource {
            user: crate::http::resources::account::AccountResource::from(&account),
            usage,
        };
        let value: serde_json::Value = serde_json::to_value(&row).expect("serialize");

        assert_eq!(value["user"]["email"], "row@example.com");
        assert!(
            value.get("last_24h").is_some(),
            "the usage is flattened beside it: {value}"
        );
    }

    #[test]
    fn my_own_usage_is_flat_beneath_my_identity() {
        // The same shape as a row, and what the generated types are built from.
        let u = store("me");
        let (span, now) = day();
        let me = MeUsageResource {
            account: AccountSummary {
                name: "Ada".to_owned(),
                email: "ada@example.com".to_owned(),
            },
            usage: UsageResource::of(&u, "acct", span, now, None),
        };
        let value: serde_json::Value = serde_json::to_value(&me).expect("serialize");

        assert_eq!(value["account"]["email"], "ada@example.com");
        assert!(value.get("all_time").is_some(), "{value}");
        assert!(value.get("series_hourly").is_some(), "{value}");
    }

    #[test]
    fn the_report_names_the_window_in_the_caller_s_own_words() {
        // "24h" and "1d" are the same span and not the same string, and the UI
        // echoes back what was asked for.
        let u = store("report");
        let now = now_secs();
        let span = Window::parse("7d").expect("a known window").span(now);
        let report = UsageReportResource {
            window: "7d".to_owned(),
            hours: span.hours(),
            window_start: crate::now_rfc3339_at(span.first),
            window_end: crate::now_rfc3339_at(span.last),
            users: Vec::new(),
        };

        assert_eq!(report.window, "7d");
        assert_eq!(report.hours, 24 * 7);
        assert_eq!(report.users.len(), 0, "an installation with no accounts says so");
        let _ = UsageResource::of(&u, "acct", span, now, None);
    }
}
