// SPDX-License-Identifier: MPL-2.0

//! The lease resource (`claim` / `release`).
//!
//! The daemon is the arbiter because it is already the single owner of this
//! host — one process, guaranteed by an exclusive lock on the state directory.
//! That is what makes leases correct here without any agreement protocol: two
//! agents asking at the same moment are two requests to one mutex.

use std::io::Write;

use super::{error_json, respond_json};

/// `POST /claims` — take or renew a lease.
///
/// Answers 200 with the grant, or **409 with the current holder**. The holder
/// matters: an agent refused a task can pick different work instead of
/// spinning, which is the difference between a fleet that spreads out and one
/// that queues behind a single key.
pub(super) fn grant<W: Write>(stream: &mut W, body_bytes: &[u8]) {
    let parsed: serde_json::Value = serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
    let (Some(key), Some(holder)) = (
        parsed.get("key").and_then(|v| v.as_str()),
        parsed.get("holder").and_then(|v| v.as_str()),
    ) else {
        error_json(stream, 400, "expected {\"key\":\"…\",\"holder\":\"…\",\"ttl_ms\":…}");
        return;
    };
    let ttl = parsed
        .get("ttl_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(crate::claims::DEFAULT_TTL_MS);
    let now = crate::unix_millis();
    match crate::claims::with_registry(|r| r.grant(key, holder, ttl, now)) {
        Ok(claim) => {
            let body = serde_json::json!({ "granted": true, "claim": claim, "now_ms": now }).to_string();
            respond_json(stream, 200, &body);
        }
        Err(crate::claims::GrantError::Held(cur)) => {
            let body = serde_json::json!({ "granted": false, "claim": cur, "now_ms": now }).to_string();
            respond_json(stream, 409, &body);
        }
        Err(crate::claims::GrantError::Invalid(e)) => error_json(stream, 400, &e),
    }
}

/// `POST /claims/release` — give a lease back early.
///
/// A path rather than `DELETE /claims/<key>` because keys carry `/` (they are
/// task ids and file paths), and percent-decoding a key out of a URL is a
/// worse trade than one more route.
pub(super) fn release<W: Write>(stream: &mut W, body_bytes: &[u8]) {
    let parsed: serde_json::Value = serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null);
    let (Some(key), Some(holder)) = (
        parsed.get("key").and_then(|v| v.as_str()),
        parsed.get("holder").and_then(|v| v.as_str()),
    ) else {
        error_json(stream, 400, "expected {\"key\":\"…\",\"holder\":\"…\"}");
        return;
    };
    let now = crate::unix_millis();
    let released = crate::claims::with_registry(|r| r.release(key.trim(), holder.trim(), now));
    // Not an error either way: releasing something you no longer hold is what
    // a late agent does, and it should be a no-op rather than a failure that
    // sends it into recovery logic.
    let body = serde_json::json!({ "released": released }).to_string();
    respond_json(stream, 200, &body);
}
