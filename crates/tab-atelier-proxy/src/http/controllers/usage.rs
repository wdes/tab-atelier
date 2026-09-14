// SPDX-License-Identifier: MPL-2.0

//! The operator's usage report, across every account.

use std::sync::Arc;

use crate::http::resources;
use crate::server::State;
use crate::transport::{Reply, json_of};
use crate::usage;

/// Every account's tokens over a window, as one page.
///
/// `query` is the raw query string, because the window is a presentation choice
/// — `?window=…` — and belongs to the resource layer that already knows how to
/// spell one, not to the route table.
pub(crate) fn usage_report(state: &Arc<State>, query: &str) -> Reply {
    let now = usage::now_secs();
    // One resolution for the whole response. Calling `span` again inside
    // `UsageResource::of` would re-read the clock, and a response that straddled
    // an hour boundary would answer its totals and its series for two different
    // windows.
    let window = resources::path_window(query).unwrap_or(usage::Window::Hours(24 * 7));
    let span = window.span(now);
    // Store before usage: the same order `account::remove` takes them, so a
    // report racing a deletion cannot deadlock against it.
    let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let body = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        resources::UsageReportResource {
            window: window.token(),
            hours: span.hours(),
            window_start: crate::now_rfc3339_at(span.first),
            window_end: crate::now_rfc3339_at(span.last),
            users: store
                .accounts()
                .iter()
                .map(|a| resources::UserUsageResource {
                    user: resources::AccountResource::from(a),
                    usage: resources::UsageResource::of(&u, &a.id, span, now),
                })
                .collect(),
        }
    };
    json_of(200, &body)
}
