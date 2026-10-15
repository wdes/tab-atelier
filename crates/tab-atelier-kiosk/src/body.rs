// SPDX-License-Identifier: MPL-2.0

//! The response body type, and the two ways this crate builds one.

use bytes::Bytes;
use http::{Response, StatusCode, header};
use http_body_util::Full;

/// A response body, always a complete in-memory buffer — the page, a stub, or a
/// JSON answer. There is no streaming path: the Kiosk answers everything it
/// serves itself, and reaching a remote returns that remote's whole reply.
pub type BoxBody = Full<Bytes>;

/// A response body from bytes already in hand.
pub fn full(body: impl Into<Bytes>) -> BoxBody {
    Full::new(body.into())
}

/// A JSON response carrying `value`, for everything this crate answers itself.
pub fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(value.to_string()))
        .expect("static response builder")
}
