// SPDX-License-Identifier: MPL-2.0

//! The `/me/*` paths: a key's owner looking at their own account.
//!
//! These are the only routes a client reaches with its ordinary relay key
//! rather than the admin token, so they answer about *the caller* and take no
//! account name at all. That is deliberate: a path that took one would invite a
//! client to ask about somebody else, and the guard that answers "who is this"
//! is right there in the argument list.
//!
//! The account arrives already authenticated — [`ClientKey`] is the guard — so
//! nothing here re-reads the key or hunts for a header. That matters beyond
//! tidiness: the guard is also what records the sighting, and looking the key up
//! a second time would record it twice.
//!
//! [`ClientKey`]: crate::http::guards::ClientKey

use std::sync::Arc;

use crate::http::guards::Arrival;
use crate::http::raw::Raw;
use crate::http::resources;
use crate::server::State;
use crate::transport::{Reply, json_of};
use crate::users::Account;
use crate::{egress, usage};

/// What this account has spent, by window and by model.
///
/// Reading your own statistics is a use of the key like any other, and an agent
/// polling this is exactly the traffic somebody reviewing access wants to see —
/// which is why it goes through the same guard as a relayed request rather than
/// a cheaper read-only path.
pub(crate) fn me_usage(state: &Arc<State>, who: &Account, window: Option<&str>) -> Reply {
    let now = usage::now_secs();
    // The query arrives as Rocket's typed parameter; the parser wants the raw
    // form. Rebuilding the one key it looks for keeps a single parser rather
    // than two that could drift.
    let span = window
        .and_then(|w| resources::path_window(&format!("window={w}")))
        .unwrap_or(usage::Window::Hours(24 * 7))
        .span(now);
    let usage = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        resources::UsageResource::of(&u, &who.id, span, now)
    };
    json_of(
        200,
        &resources::MeUsageResource {
            account: resources::AccountSummary {
                name: who.display_name(),
                email: who.email.clone(),
            },
            usage,
        },
    )
}

/// Install a repaired credential, on behalf of the caller.
///
/// The credential is repaired rather than replaced: the tab's own login is
/// re-pointed at this proxy, so the secret the user already has keeps working
/// and nothing has to be handed around. It is a blocking call — it may make up
/// to two requests to Anthropic — and it is off the async runtime for that
/// reason, because a slow upstream must not stall every other tab.
///
/// A body that is not JSON, or not a credential, is refused before anything is
/// written. 409 is for the state not permitting it (a working proxy, somebody
/// else's account) as distinct from 400, "you sent rubbish" — a client can then
/// tell "nothing to fix" from "try again properly".
pub(crate) async fn me_credentials(state: &Arc<State>, who: &Account, arrival: &Arrival, body: Raw) -> Reply {
    let _ = state;
    let Ok(raw) = String::from_utf8(body.bytes().to_vec()) else {
        return crate::http::problem(400, "body is not UTF-8");
    };
    let ip = arrival.ip.clone();
    let outcome = tokio::task::spawn_blocking(move || egress::repair_credentials(&raw)).await;
    match outcome {
        Ok(Ok(imported)) => {
            log::warn!(
                "claude credential replaced via /me/credentials by {} <{}> from {ip}",
                who.display_name(),
                who.email,
            );
            let installed_for = imported.identity.map(|i| i.email).unwrap_or_default();
            json_of(200, &resources::CredentialsResource::installed(installed_for))
        }
        Ok(Err(e)) => {
            log::warn!(
                "credential repair refused for {} <{}> from {ip}: {e}",
                who.display_name(),
                who.email,
            );
            crate::http::problem(409, e)
        }
        Err(_) => crate::http::problem(500, "credential repair panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seeded account, made if it is not there.
    ///
    /// The store the tests share is persisted to disk and never cleared, so a
    /// helper that always added would start failing on the second run. Finding
    /// first makes these tests order- and history-independent.
    fn state_with_account() -> (Arc<State>, Account) {
        let state = Arc::new(State::for_tests("t".to_owned()));
        let who = {
            let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if store.find("ada@example.com").is_none() {
                store.add("Ada", "Lovelace", "ada@example.com").expect("add");
            }
            store.find("ada@example.com").cloned().expect("seeded")
        };
        (state, who)
    }

    #[test]
    fn a_usage_report_is_json_and_not_an_error() {
        let (state, who) = state_with_account();
        let reply = me_usage(&state, &who, None);
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn the_default_window_is_a_week() {
        // No window parameter means the same span a dashboard shows by default.
        // A different fallback would make two callers disagree about what the
        // numbers mean without either being wrong.
        let (state, who) = state_with_account();
        let a = me_usage(&state, &who, None);
        let b = me_usage(&state, &who, Some("7d"));
        assert_eq!(a.status, b.status);
    }

    #[test]
    fn a_window_nobody_recognises_falls_back_rather_than_failing() {
        // A typo in a query string should still return the caller's own
        // numbers, not an error page.
        let (state, who) = state_with_account();
        assert_eq!(me_usage(&state, &who, Some("nonsense")).status, 200);
    }

    #[test]
    fn a_body_that_is_not_utf8_is_a_400_not_a_panic() {
        let (state, who) = state_with_account();
        let arrival = Arrival {
            peer: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ip: "127.0.0.1".to_owned(),
            presented: String::new(),
            method: http::Method::POST,
            target: "/me/credentials".to_owned(),
            headers: http::HeaderMap::new(),
        };
        let reply = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(me_credentials(
                &state,
                &who,
                &arrival,
                Raw(bytes::Bytes::from_static(&[0xff, 0xfe])),
            ));
        assert_eq!(reply.status, 400);
    }
}
