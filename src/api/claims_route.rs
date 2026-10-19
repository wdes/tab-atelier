// SPDX-License-Identifier: MPL-2.0

//! The lease resource (`claim` / `release`).
//!
//! The daemon is the arbiter because it is already the single owner of this
//! host — one process, guaranteed by an exclusive lock on the state directory.
//! That is what makes leases correct here without any agreement protocol: two
//! agents asking at the same moment are two requests to one mutex.

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

/// A grant or a refusal, both of which carry the claim that decides it.
///
/// The refusal case is why `granted` and the holder travel together: an agent
/// refused a task can pick different work instead of spinning, which is the
/// difference between a fleet that spreads out and one that queues behind a
/// single key. The status differs (200 / 409), the shape does not.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Grant {
    granted: bool,
    claim: crate::claims::Claim,
    now_ms: u64,
}

/// Whether a release actually released anything.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Released {
    released: bool,
}

/// `POST /claims` — take or renew a lease.
///
/// Answers 200 with the grant, or **409 with the current holder**.
pub(super) fn grant(request: &Request<'_>) -> Result<Doc<Grant>, Failure> {
    let parsed: serde_json::Value = serde_json::from_slice(request.body).unwrap_or(serde_json::Value::Null);
    let (Some(key), Some(holder)) = (
        parsed.get("key").and_then(|v| v.as_str()),
        parsed.get("holder").and_then(|v| v.as_str()),
    ) else {
        return Err(Failure::problem(
            400,
            "expected {\"key\":\"…\",\"holder\":\"…\",\"ttl_ms\":…}",
        ));
    };
    let ttl = parsed
        .get("ttl_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(crate::claims::DEFAULT_TTL_MS);
    let now = crate::unix_millis();
    match crate::claims::with_registry(|r| r.grant(key, holder, ttl, now)) {
        Ok(claim) => Ok(Doc::ok(Grant {
            granted: true,
            claim,
            now_ms: now,
        })),
        Err(crate::claims::GrantError::Held(claim)) => {
            // A held lease is not a failure of the request but an answer to it:
            // 409 with the holder, so the caller can retarget rather than retry.
            // Same resource as a grant — only the code differs, which is what
            // `Doc::at` is for.
            Ok(Doc::at(
                409,
                Grant {
                    granted: false,
                    claim,
                    now_ms: now,
                },
            ))
        }
        Err(crate::claims::GrantError::Invalid(e)) => Err(Failure::problem(400, e)),
    }
}

/// `POST /claims/release` — give a lease back early.
///
/// A path rather than `DELETE /claims/<key>` because keys carry `/` (they are
/// task ids and file paths), and percent-decoding a key out of a URL is a
/// worse trade than one more route.
pub(super) fn release(request: &Request<'_>) -> Result<Doc<Released>, Failure> {
    let parsed: serde_json::Value = serde_json::from_slice(request.body).unwrap_or(serde_json::Value::Null);
    let (Some(key), Some(holder)) = (
        parsed.get("key").and_then(|v| v.as_str()),
        parsed.get("holder").and_then(|v| v.as_str()),
    ) else {
        return Err(Failure::problem(400, "expected {\"key\":\"…\",\"holder\":\"…\"}"));
    };
    let now = crate::unix_millis();
    let released = crate::claims::with_registry(|r| r.release(key.trim(), holder.trim(), now));
    // Not an error either way: releasing something you no longer hold is what
    // a late agent does, and it should be a no-op rather than a failure that
    // sends it into recovery logic.
    Ok(Doc::ok(Released { released }))
}
