// SPDX-License-Identifier: MPL-2.0

//! Shared-quota pressure, for the dashboard.

use std::sync::{Arc, MutexGuard};

use serde::Serialize;

use crate::account::{Health, Monitor, Sample};
use crate::server::State;
use crate::{routing, usage};

/// Upstream's own report about the shared plan, plus our scheduler's response.
#[derive(Serialize)]
pub(crate) struct PressureResource {
    pub plan: PlanResource,
    pub scheduler: crate::qos::SchedSnapshot,
    /// The point above which a provider stops being first choice. Routing
    /// prefers moving the work to another provider at this level; only when
    /// none is left does the class step down.
    pub strained_above: f64,
    pub providers: std::collections::BTreeMap<String, usize>,
}

/// What the plan says about itself, and what we watched it do.
#[derive(Serialize)]
pub(crate) struct PlanResource {
    /// The honest signal: upstream's own report, not an inference from our
    /// accounting. `null` when the monitor has gone quiet — `health` says why,
    /// and the UI must show that rather than the last number it saw.
    pub utilization: Option<f64>,
    pub latest: Option<Sample>,
    pub health: Health,
    /// What upstream claims about the weekly window, and what we actually
    /// watched it do. They disagree, routinely — see `Sample::seven_day_resets`
    /// — so the UI shows both rather than choosing one to present as fact.
    pub weekly_last_drop: Option<WeeklyDropResource>,
    pub history: Vec<Sample>,
}

/// The last time the weekly counter was seen to fall, and by how much.
#[derive(Serialize)]
pub(crate) struct WeeklyDropResource {
    pub ts: String,
    pub from: f64,
    pub to: f64,
}

impl PressureResource {
    /// Read the current pressure out of the live state.
    ///
    /// `now` is a parameter rather than a call to the clock so the shape can be
    /// tested without waiting for a window to roll over.
    ///
    /// One small scope per lock, none held across another and none held while
    /// the value is serialized: this is on the request path of the dashboard,
    /// and a lock held across a socket write is how a reader of the registry
    /// ends up waiting on someone's slow connection.
    #[must_use]
    pub(crate) fn of(state: &Arc<State>, now: u64) -> Self {
        let plan = {
            let acct: MutexGuard<'_, Monitor> = state.account.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            PlanResource {
                utilization: acct.utilization(),
                latest: acct.latest().cloned(),
                health: acct.health(usage::now_secs()),
                weekly_last_drop: acct
                    .last_weekly_drop()
                    .map(|(ts, from, to)| WeeklyDropResource { ts, from, to }),
                history: acct.recent().to_vec(),
            }
        };
        let scheduler = {
            let sched = state.sched.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            sched.snapshot(now)
        };
        let providers = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .summary();
        Self {
            plan,
            scheduler,
            strained_above: routing::STRAINED_ABOVE,
            providers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<State> {
        Arc::new(State::for_tests("t".to_owned()))
    }

    #[test]
    fn the_report_publishes_the_threshold_the_router_actually_uses() {
        // If these ever diverge the dashboard draws a line the proxy does not
        // follow, and the operator reads it as fact.
        let report = PressureResource::of(&state(), 1_700_000_000_000);
        assert!(
            (report.strained_above - routing::STRAINED_ABOVE).abs() < f64::EPSILON,
            "the report says {} and the router uses {}",
            report.strained_above,
            routing::STRAINED_ABOVE
        );
    }

    #[test]
    fn a_monitor_that_has_not_reported_shows_nothing_rather_than_a_zero() {
        // A zero would render as "nothing is shared", which is the opposite of
        // "we do not know". The health field is what says which it is.
        let report = PressureResource::of(&state(), 1_700_000_000_000);
        assert!(report.plan.utilization.is_none());
        assert!(report.plan.latest.is_none());
        assert!(report.plan.history.is_empty());
    }

    #[test]
    fn the_scheduler_section_is_present_even_with_no_traffic() {
        // The panel is drawn from this, so an absent section is a broken page
        // rather than a quiet one.
        let report = PressureResource::of(&state(), 1_700_000_000_000);
        assert_eq!(report.scheduler.admitted, 0);
        assert_eq!(report.scheduler.rejected, 0);
        assert!(report.scheduler.accounts.is_empty());
    }

    #[test]
    fn the_providers_section_is_keyed_by_provider_id() {
        // The UI looks providers up by id, so a positional list would silently
        // mislabel every row when one is added or removed.
        let report = PressureResource::of(&state(), 1_700_000_000_000);
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"providers\""), "{json}");
        assert!(json.contains("\"plan\""), "{json}");
        assert!(json.contains("\"scheduler\""), "{json}");
    }
}
