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

    /// The JSON body of a reply that is a buffer.
    ///
    /// Every one of these responses is JSON, and a test that had to unwrap it
    /// itself would spend more lines on the unwrapping than on what it checks.
    fn json(reply: &crate::transport::Reply) -> serde_json::Value {
        match &reply.body {
            crate::transport::ReplyBody::Bytes(bytes) => serde_json::from_slice(bytes).expect("a JSON body"),
            crate::transport::ReplyBody::Stream(_) => {
                panic!("these replies are buffered, never streamed")
            }
        }
    }

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

    /// The read path the panel polls, which nothing was exercising.
    ///
    /// It is a whole response built out of three fields, and the countdown is
    /// the one with arithmetic in it.
    #[test]
    fn the_status_reports_whether_the_recorder_is_on() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        let off = status(&state);
        assert_eq!(off.status, 200);

        assert_eq!(arm(&ArmInspect { minutes: 5 }, &state).status, 200);
        let on = status(&state);
        assert_eq!(on.status, 200);

        assert_eq!(json(&off)["armed"], false);
        assert_eq!(json(&on)["armed"], true);
    }

    /// The countdown is a subtraction the server does, not two clocks the
    /// browser has to reconcile — so it must never underflow when the window
    /// has passed.
    #[test]
    fn the_countdown_never_goes_negative() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        // Armed and then left: the moment the status is read is well past the
        // expiry, which is what a page left open overnight does.
        {
            let mut ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ins.arm(0, 1);
        }
        let value = json(&status(&state));
        assert_eq!(value["armed"], false, "a window that has passed is not on");
        assert_eq!(
            value["seconds_left"], 0,
            "and the countdown floors at zero rather than wrapping: {value}"
        );
    }

    /// The ceiling on a window is published, so the page can bound its own
    /// input instead of offering a number the server will clamp.
    #[test]
    fn the_status_publishes_the_longest_window_the_api_accepts() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        let value = json(&status(&state));
        assert_eq!(value["max_arm_minutes"], crate::inspect::MAX_ARM_MINUTES);
    }

    /// An arm reports when it will end, so the caller that armed it does not
    /// have to guess or poll.
    #[test]
    fn arming_answers_with_the_moment_it_will_stop() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        let value = json(&arm(&ArmInspect { minutes: 5 }, &state));
        let until = value["armed_until"].as_u64().expect("a Unix second");
        assert!(until > crate::usage::now_secs(), "the window is in the future");
    }

    /// Disarming clears what was caught.
    ///
    /// "Stop recording" and "and delete what you recorded" are the same
    /// intention, and a capture holds a prompt — so leaving them behind after
    /// the operator asked to stop would keep the most sensitive thing this
    /// process ever holds on disk for no reason.
    #[test]
    fn disarming_forgets_what_was_caught() {
        let state = Arc::new(State::for_tests("s3cret".to_owned()));
        let held = state
            .inspect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent()
            .len();

        assert_eq!(disarm(&state).status, 200);

        let after = state
            .inspect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent()
            .len();
        assert_eq!(after, 0, "nothing is held after a disarm (was {held})");
    }

    #[test]
    fn an_arming_request_without_minutes_uses_its_default() {
        // The default is not zero: POSTing an empty body is how the UI's
        // "arm" button arrives, and arming for no time would look broken.
        let req: ArmInspect = serde_json::from_slice(b"{}").expect("empty body is valid");
        assert!(req.minutes() > 0);
    }
}
