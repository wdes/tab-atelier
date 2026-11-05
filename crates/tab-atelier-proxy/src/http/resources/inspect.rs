// SPDX-License-Identifier: MPL-2.0

//! What the request recorder is doing, and what it caught.
//!
//! A capture holds a prompt, so the shape here is deliberately the only way it
//! leaves the process: the same [`Capture`] the store kept, serialised, not a
//! summary that a caller could mistake for harmless.
//!
//! The one exception is [`CaptureSummary`], and it exists because the list was
//! carrying every body: see its documentation.

use serde::Serialize;
use tab_atelier_api::schemars;

use crate::inspect::{Capture, Compaction};
use crate::users::Client;

/// The recorder's state and the captures it is holding.
///
/// `seconds_left` is computed rather than left to the caller: the page that
/// shows it is a browser, and making it subtract two clocks is how a countdown
/// drifts.
#[derive(Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(crate) struct InspectStatusResource {
    /// Whether the recorder is on right now.
    pub armed: bool,
    /// When it turns itself off, as a Unix second. Zero when disarmed.
    pub armed_until: u64,
    /// `armed_until` minus now, floored at zero.
    pub seconds_left: u64,
    /// The longest arm the API accepts, so the page can bound its own input.
    pub max_arm_minutes: u64,
    /// Everything caught, newest last — as rows, without their bytes.
    pub captures: Vec<CaptureSummary>,
}

/// One capture, as the list draws it: the row, and none of the payload.
///
/// The list used to carry every request body and reply excerpt it held. With
/// [`crate::inspect::KEEP`] captures of up to 4 MiB each, opening the panel
/// meant fetching and parsing around a hundred megabytes of prompts to draw a
/// table of 25 rows — seconds of waiting, for bytes that nothing on that table
/// reads, including the reply excerpt that the panel never showed at all.
///
/// So the list is a summary and the body is fetched one capture at a time from
/// `GET /api/inspect/{id}`. That is also what [`Self::body_bytes`] is for: a
/// row can say how large a body is without holding it.
#[derive(Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(crate) struct CaptureSummary {
    /// The id to name in `GET /api/inspect/{id}`. See [`Capture::id`].
    pub id: u64,
    /// RFC3339.
    pub ts: String,
    pub account_id: String,
    pub account_email: String,
    pub method: String,
    /// Path and query as sent upstream.
    pub path: String,
    /// Where it went, after routing.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<Client>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool: Option<String>,
    pub kind: crate::classifier::Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<crate::usage::Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<crate::usage::Cost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,
    /// True when the body was cut at [`crate::inspect::MAX_BODY`], so a row can
    /// say so before the click that would otherwise look like a corrupt file.
    pub request_truncated: bool,
    /// The request body's length in bytes, which is the size of what the detail
    /// call will return.
    pub body_bytes: usize,
}

impl CaptureSummary {
    /// The row for one captured call.
    #[must_use]
    pub(crate) fn of(c: &Capture) -> Self {
        Self {
            id: c.id,
            ts: c.ts.clone(),
            account_id: c.account_id.clone(),
            account_email: c.account_email.clone(),
            method: c.method.clone(),
            path: c.path.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            client: c.client.clone(),
            server_tool: c.server_tool.clone(),
            kind: c.kind,
            status: c.status,
            tokens: c.tokens,
            cost: c.cost,
            compaction: c.compaction.clone(),
            request_truncated: c.request_truncated,
            body_bytes: c.request_body.len(),
        }
    }
}

/// The reply to an arm: when it will turn itself off.
#[derive(Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(crate) struct InspectStateResource {
    /// The Unix second at which the recorder disarms itself.
    pub armed_until: u64,
}
