// SPDX-License-Identifier: MPL-2.0

//! Usage, rolled up over a window.
//!
//! These are named structs rather than hand-built `json!` values because the
//! `OpenAPI` document is generated from them: a field that exists only inside a
//! macro would be missing from the spec the UI is written against.

use std::collections::BTreeMap;

use serde::Serialize;

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
}

impl WindowResource {
    fn of(u: &usage::Store, id: &str, window: usage::Window, now: u64) -> Self {
        let (calls, errors, tokens) = u.totals(id, window.span(now));
        Self {
            calls,
            errors,
            tokens: TokenResource::from(&tokens),
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
}

impl From<&usage::Bucket> for BucketResource {
    fn from(b: &usage::Bucket) -> Self {
        Self {
            hour: b.hour,
            calls: b.calls,
            errors: b.errors,
            input: b.tokens.input,
            output: b.tokens.output,
            cache_read: b.tokens.cache_read,
            cache_write: b.tokens.cache_write,
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
}

/// Render one account's usage.
///
/// `now` is passed rather than read here, so the requested window and the two
/// fixed ones are resolved from one reading of the clock.
impl UsageResource {
    #[must_use]
    pub(crate) fn of(u: &usage::Store, id: &str, span: usage::Span, now: u64) -> Self {
        Self {
            window: SpanResource {
                start: crate::now_rfc3339_at(span.first),
                end: crate::now_rfc3339_at(span.last),
                hours: span.hours(),
            },
            all_time: WindowResource::of(u, id, usage::Window::All, now),
            last_24h: WindowResource::of(u, id, usage::Window::Hours(24), now),
            last_7d: WindowResource::of(u, id, usage::Window::Hours(24 * 7), now),
            by_model: u
                .for_account(id)
                .map(|a| {
                    a.by_model
                        .iter()
                        .map(|(m, t)| (m.clone(), TokenResource::from(t)))
                        .collect()
                })
                .unwrap_or_default(),
            series_hourly: u.series(id, span).iter().map(BucketResource::from).collect(),
            retained_hours: usage::RETAIN_HOURS,
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
