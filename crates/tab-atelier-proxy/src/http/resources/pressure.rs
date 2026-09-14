// SPDX-License-Identifier: MPL-2.0

//! Shared-quota pressure, for the dashboard.

use std::sync::{Arc, MutexGuard};

use serde::Serialize;

use crate::account::{Health, Monitor, Sample};
use crate::server::{State, now_ms};
use crate::transport::{Reply, json};
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

/// What the dashboard shows about pressure.
pub(crate) fn pressure_json(state: &Arc<State>) -> Reply {
    let now = now_ms();
    // One tiny scope per lock: both are on the request path, so neither is
    // held across the other or across building the response.
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
    let body = PressureResource {
        plan,
        scheduler,
        strained_above: routing::STRAINED_ABOVE,
        providers,
    };
    json(200, &serde_json::to_string(&body).unwrap_or_default())
}
