// SPDX-License-Identifier: MPL-2.0

//! The response body type, and the two ways this crate builds one.

use bytes::Bytes;
use http::{Response, StatusCode, header};
use http_body_util::combinators::BoxBody as BoxBodyInner;
use http_body_util::{BodyExt as _, Full};

/// A body that is either a complete in-memory buffer (the page, a stub, an
/// error) or a stream fed by the upstream hop (a proxied response). Both are
/// `Send` and hyper-ready, which is why the two paths can share one type.
pub type BoxBody = BoxBodyInner<Bytes, std::io::Error>;

/// A response body from bytes already in hand.
pub fn full(body: impl Into<Bytes>) -> BoxBody {
    // `Full`'s error type is `Infallible`; the proxy stream's is `io::Error`.
    // Boxed, they are the same body, and an infallible one never yields the
    // second variant, so the match is unreachable by construction.
    Full::new(body.into()).map_err(|never| match never {}).boxed()
}

/// A JSON response carrying `value`, for everything this crate answers itself
/// rather than relays: the two error bodies, and nothing else today.
pub fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(value.to_string()))
        .expect("static response builder")
}
