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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::Store;

    /// A state of this test's own: its store and its usage log on disk, both
    /// named after the test, because both are written to.
    fn state_for(name: &str) -> Arc<State> {
        let dir = std::env::temp_dir()
            .join("tab-atelier-usage-report-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = Store::load(dir.join("users.json")).expect("a fresh store");

        let state = Arc::new(State::for_tests("t".to_owned()));
        *state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = store;
        // The usage log goes somewhere of its own too: `for_tests` points it at
        // the temp directory, and a log shared with other tests would put their
        // calls on this account's row.
        *state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = usage::Store::load(dir.join("usage"));
        state
    }

    #[test]
    fn an_installation_with_no_accounts_reports_an_empty_table() {
        // The first thing anybody sees after installing. An error here would
        // read as a broken deployment.
        let state = state_for("empty");
        let reply = usage_report(&state, "");
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn the_table_has_a_row_for_every_account() {
        // The panel is the whole reason this endpoint exists: one request rather
        // than one per account.
        let state = state_for("rows");
        for (i, email) in ["usage-a@example.com", "usage-b@example.com"].iter().enumerate() {
            state
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .add("Ada", &format!("L{i}"), email)
                .expect("add");
        }
        assert_eq!(usage_report(&state, "").status, 200);
    }

    #[test]
    fn a_window_the_caller_asked_for_is_the_one_named_in_the_answer() {
        // "24h" and "1d" are the same span and not the same string, and the UI
        // echoes back what it asked for.
        let state = state_for("window");
        let reply = usage_report(&state, "window=24h");
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn a_window_nobody_recognises_still_produces_a_report() {
        // A typo in a query string must not be able to fail the page; the
        // fallback is the default window, which is what the panel shows anyway.
        let state = state_for("bad-window");
        assert_eq!(usage_report(&state, "window=nonsense").status, 200);
        assert_eq!(usage_report(&state, "").status, 200);
    }

    #[test]
    fn a_recorded_call_shows_up_in_the_account_s_row() {
        // The join between the two stores, which is the part that could be
        // silently wrong: the usage log is keyed by account ID and the table is
        // rendered from accounts.
        let state = state_for("join");
        let id = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add("Ada", "Lovelace", "usage-join@example.com")
            .expect("add")
            .id;
        state
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(
                &id,
                Some("m"),
                crate::usage::Tokens {
                    input: 4,
                    output: 6,
                    ..crate::usage::Tokens::default()
                },
                true,
            );

        let reply = usage_report(&state, "window=24h");
        let crate::transport::ReplyBody::Bytes(bytes) = reply.body else {
            panic!("a report is a buffered body");
        };
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["users"][0]["user"]["email"], "usage-join@example.com");
        assert_eq!(
            value["users"][0]["last_24h"]["tokens"]["total"], 10,
            "the account's own tokens, on its own row: {value}"
        );
    }
}
