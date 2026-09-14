// SPDX-License-Identifier: MPL-2.0

//! What the request recorder is doing, and what it caught.
//!
//! A capture holds a prompt, so the shape here is deliberately the only way it
//! leaves the process: the same [`Capture`] the store kept, serialised, not a
//! summary that a caller could mistake for harmless.

use serde::Serialize;

use crate::inspect::Capture;

/// The recorder's state and the captures it is holding.
///
/// `seconds_left` is computed rather than left to the caller: the page that
/// shows it is a browser, and making it subtract two clocks is how a countdown
/// drifts.
#[derive(Serialize)]
pub(crate) struct InspectStatusResource {
    /// Whether the recorder is on right now.
    pub armed: bool,
    /// When it turns itself off, as a Unix second. Zero when disarmed.
    pub armed_until: u64,
    /// `armed_until` minus now, floored at zero.
    pub seconds_left: u64,
    /// The longest arm the API accepts, so the page can bound its own input.
    pub max_arm_minutes: u64,
    /// Everything caught, newest last.
    pub captures: Vec<Capture>,
}

/// The reply to an arm: when it will turn itself off.
#[derive(Serialize)]
pub(crate) struct InspectStateResource {
    /// The Unix second at which the recorder disarms itself.
    pub armed_until: u64,
}
