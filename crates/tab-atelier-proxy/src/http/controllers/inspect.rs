// SPDX-License-Identifier: MPL-2.0

//! Request inspection: arming the recorder, and reading what it caught.
//!
//! A capture is a prompt, so this is admin-only — a user key must never be able
//! to read one, not even for its own account. The guard lives in
//! [`crate::http::middleware::admin_token`]; what is here is the switch and the
//! list.

use std::sync::Arc;

use crate::http::requests::inspect::ArmInspect;
use crate::http::resources::{InspectStateResource, InspectStatusResource};
use crate::server::State;
use crate::transport::{Reply, json_of};

/// What the recorder is doing, and everything it has caught.
pub(crate) fn status(state: &Arc<State>) -> Reply {
    let ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = crate::usage::now_secs();
    json_of(
        200,
        &InspectStatusResource {
            armed: ins.armed(now),
            armed_until: ins.armed_until(),
            seconds_left: ins.armed_until().saturating_sub(now),
            max_arm_minutes: crate::inspect::MAX_ARM_MINUTES,
            captures: ins.recent().to_vec(),
        },
    )
}

/// Start recording, for the requested number of minutes.
///
/// The log line is a warning, not information: from here on the proxy holds
/// prompts it was not holding before, and the operator who armed it should be
/// able to find out when.
pub(crate) fn arm(req: &ArmInspect, state: &Arc<State>) -> Reply {
    let (minutes, until) = {
        let mut ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let minutes = req.minutes();
        (minutes, ins.arm(crate::usage::now_secs(), minutes))
    };
    log::warn!(
        "proxy: request inspection ARMED for {minutes} min, until {} — captures contain prompts",
        crate::now_rfc3339_at(until)
    );
    json_of(200, &InspectStateResource { armed_until: until })
}

/// Stop recording and forget everything caught.
///
/// One button, because "stop recording" and "and delete what you recorded" are
/// the same intention in practice.
pub(crate) fn disarm(state: &Arc<State>) -> Reply {
    let cleared = {
        let mut ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ins.disarm();
        ins.clear()
    };
    match cleared {
        Ok(()) => {
            log::info!("proxy: request inspection disarmed and captures cleared");
            json_of(200, &crate::http::resources::OkResource { ok: true })
        }
        Err(e) => crate::http::problem(500, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::State;

    #[test]
    fn arming_then_disarming_leaves_the_recorder_off() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        let armed = arm(&ArmInspect { minutes: 5 }, &state);
        assert_eq!(armed.status, 200);
        assert!(
            state
                .inspect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .armed(crate::usage::now_secs())
        );
        assert_eq!(disarm(&state).status, 200);
        assert!(
            !state
                .inspect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .armed(crate::usage::now_secs())
        );
    }

    #[test]
    fn an_arming_request_without_minutes_uses_its_default() {
        // The default is not zero: POSTing an empty body is how the UI's
        // "arm" button arrives, and arming for no time would look broken.
        let req: ArmInspect = serde_json::from_slice(b"{}").expect("empty body is valid");
        assert!(req.minutes() > 0);
    }
}
