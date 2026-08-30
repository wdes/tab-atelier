// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request authorization gate for the local API — the "GATE" half of
//! `handle_connection`'s GATE-then-DISPATCH shape (Phase B split).
//!
//! [`authorize`] runs BEFORE route dispatch, so the resource handlers never
//! re-check auth: if execution reaches a handler, this gate already accepted
//! the request at the right level. Extracted verbatim from the old inline gate
//! (behavior-preserving); the token/cert *bootstrap* loaders still live in the
//! parent module and are reachable here via `super::` if needed.

use std::sync::{Arc, Mutex};

use super::{TabSnapshot, constant_time_eq};

/// The gate's verdict for one request.
pub(super) enum Gate {
    /// Authorised — proceed to dispatch.
    Allow,
    /// Rejected — the caller writes a content-negotiated error and closes.
    Deny { status: u16, msg: &'static str },
}

/// Permission gate, in order:
///
/// 1. Master token (`api.token`) — full access to every route, no scoping.
/// 2. Global dashboard share-token — read-only fleet observability: the two
///    dashboard routes, AND every tab's read-only viewer routes (same perimeter
///    as a per-tab `share_token_ro`).
/// 3. Per-tab share token, recognised only on `/tabs/by-id/{uuid}/...`. RW
///    grants everything; RO grants reads but is refused on input/inbox/files-
///    POST with 403 (a read-only link can't be promoted to interactive).
///
/// Verbatim port of the old inline gate — same constant-time comparisons and
/// the same `touch()` activity bump inside the auth lock on success.
pub(super) fn authorize(
    state: &Arc<Mutex<TabSnapshot>>,
    method: &str,
    path: &str,
    provided_token: Option<&str>,
) -> Gate {
    // The master token lives on the shared snapshot (not a per-connection clone)
    // so `POST /master-token/reset` can hot-swap it. The non-empty guard means
    // an as-yet-uninitialised master ("") never authorises a token-less request.
    let is_master = {
        let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ok = !snap.master_token.is_empty()
            && constant_time_eq(provided_token.unwrap_or("").as_bytes(), snap.master_token.as_bytes());
        if ok {
            snap.touch();
        }
        ok
    };
    // Global dashboard share-token — a READ-ONLY observability credential for the
    // whole fleet. Authorises the dashboard routes AND every tab's read-only
    // viewer routes; folded into the per-tab RO verdict below.
    let dashboard_matches = !is_master && {
        let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ok = !snap.dashboard_share_token.is_empty()
            && constant_time_eq(
                provided_token.unwrap_or("").as_bytes(),
                snap.dashboard_share_token.as_bytes(),
            );
        if ok {
            snap.touch();
        }
        ok
    };
    let is_dashboard_token =
        dashboard_matches && matches!(path, "/dashboard" | "/dashboard/state" | "/dashboard/activity");
    if is_master || is_dashboard_token {
        return Gate::Allow;
    }

    let allowed = if let Some(p) = provided_token
        && let Some(rest) = path.strip_prefix("/tabs/by-id/")
        && let Some((uuid, action)) = rest.split_once('/')
        && matches!(
            action,
            "view" | "output" | "stream" | "input" | "files" | "outbox" | "inbox"
        ) {
        let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let verdict = state_g.tabs.iter().find(|t| &*t.id == uuid).and_then(|t| {
            // Constant-time per-byte comparison so a brute-force probe can't shave
            // bits off the search space by timing the reject (audit #2).
            let rw_match = !t.share_token_rw.is_empty() && constant_time_eq(t.share_token_rw.as_bytes(), p.as_bytes());
            // The global dashboard token acts as a read-only share token on ANY
            // tab, so a match here grades exactly like an RO link.
            let ro_match = dashboard_matches
                || (!t.share_token_ro.is_empty() && constant_time_eq(t.share_token_ro.as_bytes(), p.as_bytes()));
            // Mutating + privileged-read share-token actions require RW.
            let needs_rw = matches!(action, "input" | "inbox") || (action == "files" && method == "POST");
            if needs_rw {
                if rw_match {
                    Some(true)
                } else if ro_match {
                    Some(false)
                } else {
                    None
                }
            } else if rw_match || ro_match {
                Some(true)
            } else {
                None
            }
        });
        if verdict == Some(true) {
            state_g.touch();
        }
        verdict
    } else {
        None
    };
    match allowed {
        Some(true) => Gate::Allow,
        Some(false) => Gate::Deny {
            status: 403,
            msg: "share token is read-only",
        },
        None => Gate::Deny {
            status: 401,
            msg: "invalid or missing token",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_allow(g: &Gate) -> bool {
        matches!(g, Gate::Allow)
    }
    fn deny_status(g: &Gate) -> Option<u16> {
        match g {
            Gate::Deny { status, .. } => Some(*status),
            Gate::Allow => None,
        }
    }

    /// A snapshot with a master token, a dashboard obs token, and one tab
    /// carrying both a RW and a RO per-tab share token.
    fn fixture() -> Arc<Mutex<TabSnapshot>> {
        let mut tab = super::super::test_snapshot_tab("tab-a", "shell");
        tab.share_token_rw = "rw-tok".into();
        tab.share_token_ro = "ro-tok".into();
        let mut snap = super::super::test_snapshot(vec![tab]);
        snap.master_token = "master-secret".into();
        snap.dashboard_share_token = "dash-secret".into();
        Arc::new(Mutex::new(snap))
    }

    #[test]
    fn master_token_valid_invalid_absent() {
        let s = fixture();
        // Valid master → full access, even on a mutating per-tab route.
        assert!(is_allow(&authorize(&s, "GET", "/tabs", Some("master-secret"))));
        assert!(is_allow(&authorize(
            &s,
            "POST",
            "/tabs/by-id/tab-a/input",
            Some("master-secret")
        )));
        // Wrong token / no token on a gated route → 401.
        assert_eq!(deny_status(&authorize(&s, "GET", "/tabs", Some("nope"))), Some(401));
        assert_eq!(deny_status(&authorize(&s, "GET", "/tabs", None)), Some(401));
        // An empty master ("") must never authorise a token-less request.
        {
            let s2 = fixture();
            s2.lock().unwrap().master_token = String::new();
            assert_eq!(deny_status(&authorize(&s2, "GET", "/tabs", None)), Some(401));
            assert_eq!(deny_status(&authorize(&s2, "GET", "/tabs", Some(""))), Some(401));
        }
    }

    #[test]
    fn dashboard_obs_token_is_read_only_across_the_fleet() {
        let s = fixture();
        // Authorises the dashboard routes...
        assert!(is_allow(&authorize(&s, "GET", "/dashboard", Some("dash-secret"))));
        assert!(is_allow(&authorize(&s, "GET", "/dashboard/state", Some("dash-secret"))));
        // ...and any tab's READ routes (acts as a per-tab RO link)...
        assert!(is_allow(&authorize(
            &s,
            "GET",
            "/tabs/by-id/tab-a/view",
            Some("dash-secret")
        )));
        assert!(is_allow(&authorize(
            &s,
            "GET",
            "/tabs/by-id/tab-a/output",
            Some("dash-secret")
        )));
        // ...but is REFUSED (403, read-only) on input — no promotion to interactive.
        assert_eq!(
            deny_status(&authorize(&s, "POST", "/tabs/by-id/tab-a/input", Some("dash-secret"))),
            Some(403)
        );
        // A dashboard token on a NON-dashboard, NON-tab-scoped route → 401.
        assert_eq!(
            deny_status(&authorize(&s, "GET", "/tabs", Some("dash-secret"))),
            Some(401)
        );
    }

    #[test]
    fn per_tab_rw_vs_ro_scope() {
        let s = fixture();
        // RW token: reads AND input pass.
        assert!(is_allow(&authorize(
            &s,
            "GET",
            "/tabs/by-id/tab-a/view",
            Some("rw-tok")
        )));
        assert!(is_allow(&authorize(
            &s,
            "POST",
            "/tabs/by-id/tab-a/input",
            Some("rw-tok")
        )));
        // RO token: reads pass, input is refused with 403 (not 401).
        assert!(is_allow(&authorize(
            &s,
            "GET",
            "/tabs/by-id/tab-a/view",
            Some("ro-tok")
        )));
        assert_eq!(
            deny_status(&authorize(&s, "POST", "/tabs/by-id/tab-a/input", Some("ro-tok"))),
            Some(403)
        );
        // RO token is also refused on inbox enumeration (RW-only privileged read).
        assert_eq!(
            deny_status(&authorize(&s, "GET", "/tabs/by-id/tab-a/inbox", Some("ro-tok"))),
            Some(403)
        );
        // A share token only works on ITS tab / the share routes — an unknown
        // uuid or a non-share route → 401.
        assert_eq!(
            deny_status(&authorize(&s, "GET", "/tabs/by-id/other/view", Some("rw-tok"))),
            Some(401)
        );
        assert_eq!(deny_status(&authorize(&s, "GET", "/tabs", Some("rw-tok"))), Some(401));
    }
}
