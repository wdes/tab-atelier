// SPDX-License-Identifier: MPL-2.0

//! How a [`Reply`] becomes a response for a server that keeps hyper.
//!
//! [`crate::respond`] is the Rocket edge; this is the other one. A server that
//! owns its listener — the daemon does, because its viewer arrives as a
//! WebSocket upgrade and its relay leaves as an SSE stream, and Rocket's
//! listener hands neither over — still answers with the same [`Reply`], and it
//! is rendered here rather than by hand.
//!
//! The rules are the same on both edges, deliberately: what a reply's content
//! type turns out to be, and what happens when it has not said, must not depend
//! on the framework underneath. A handler that is moved from one server to the
//! other must answer byte for byte what it answered before.
//!
//! A streamed body needs no adapter on this edge. Rocket wanted an
//! `AsyncRead`, and so needed [`crate::respond::BodyReader`] to turn frames into
//! reads; hyper wants a body, and a [`ReplyBody::Stream`] already is one.

use http_body_util::BodyExt as _;
use http_body_util::Full;
use hyper::Response;
use hyper::StatusCode;
use hyper::header::{CONTENT_TYPE, HeaderName, HeaderValue};

use crate::marshal::{Body, Reply, ReplyBody};

impl From<Reply> for Response<Body> {
    /// Render a reply, adding nothing the reply did not ask for except the
    /// content type of a body that never said what it is holding.
    fn from(reply: Reply) -> Self {
        // Whether the reply already says what it is holding. An asset route
        // builds a reply carrying the type it worked out from the file
        // extension, and an adapter that overrode that — as the Rocket edge
        // once did with `application/json` on every buffered body — made the
        // browser refuse the UI's own scripts and rendered the page blank. A
        // reply that knows what it holds has to win, on this edge too.
        let typed = reply
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        let streamed = matches!(&reply.body, ReplyBody::Stream(_));

        let mut response = Self::new(body_of(reply.body));
        *response.status_mut() = code_of(reply.status);
        for (name, value) in reply.headers {
            // Names come from the reply layer as `&'static str`, so a rejected
            // one means a bug upstream rather than a bad request here; dropping
            // it beats failing, which would turn a typo into a 500.
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                // Appended, not inserted: a reply that carries the same header
                // twice means it, and `insert` would silently keep the last.
                response.headers_mut().append(name, value);
            }
        }
        if !typed {
            // The default is a guess, and it is the same guess the Rocket edge
            // makes: JSON is right for this API's answers, a stream is an event
            // stream. Both are applied only because the reply did not decide.
            let fallback = if streamed {
                "text/event-stream"
            } else {
                "application/json"
            };
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static(fallback));
        }
        response
    }
}

/// The body, boxed so both shapes erase to the same type.
///
/// A buffered body is [`Full`], which knows its length — so hyper sets a
/// `Content-Length` rather than falling back to chunked framing, which is what
/// the hand-written responses this replaces always sent.
fn body_of(body: ReplyBody) -> Body {
    match body {
        ReplyBody::Bytes(bytes) => Full::new(bytes).boxed(),
        ReplyBody::Stream(body) => body,
    }
}

/// The status, as hyper's type.
///
/// A reply speaks `u16` so it stays free of any one framework, and this is where
/// that becomes a status hyper can send. A code hyper cannot express is a bug
/// upstream; answering 500 says so in the only way left at this point in the
/// response, rather than panicking inside a request.
fn code_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    // `boxed` comes from `BodyExt`; `size_hint` is on `Body` itself, so the
    // trait has to be named too — `BodyExt: Body` does not put it in scope.
    use http_body::Body as _;
    use http_body_util::BodyExt as _;
    use hyper::StatusCode;
    use hyper::header;

    use super::{Reply, code_of};
    use crate::marshal::{json, text};

    fn content_type(response: &hyper::Response<crate::marshal::Body>) -> String {
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn a_buffered_reply_that_says_nothing_is_json() {
        let response: hyper::Response<_> = json(200, r#"{"ok":true}"#).into();
        assert_eq!(content_type(&response), "application/json");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_reply_that_knows_its_type_keeps_it() {
        // The regression this edge exists to not repeat: an asset route's reply
        // carrying `text/html`, overridden with `application/json` by the
        // adapter, which with `nosniff` made the browser refuse to run the UI.
        let reply = Reply::bytes(200, bytes::Bytes::from_static(b"<html></html>"))
            .with_header("content-type", "text/html".to_owned());
        let response: hyper::Response<_> = reply.into();
        assert_eq!(content_type(&response), "text/html");
    }

    #[test]
    fn a_streamed_reply_that_says_nothing_is_an_event_stream() {
        let stream = http_body_util::Full::new(bytes::Bytes::new()).boxed();
        let response: hyper::Response<_> = Reply::stream(200, stream).into();
        assert_eq!(content_type(&response), "text/event-stream");
    }

    #[test]
    fn a_buffered_body_has_a_length_rather_than_chunked_framing() {
        let response: hyper::Response<_> = text(200, "hi").into();
        assert_eq!(response.body().size_hint().exact(), Some(2));
    }

    #[test]
    fn every_header_is_kept_including_a_repeated_one() {
        // A map would keep only the last of a repeated name, which is why the
        // reply carries a `Vec` rather than a `HeaderMap`.
        let reply = text(200, "x")
            .with_header("x-a", "1".to_owned())
            .with_header("x-a", "2".to_owned());
        let response: hyper::Response<_> = reply.into();
        let seen: Vec<&str> = response
            .headers()
            .get_all("x-a")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(seen, ["1", "2"]);
    }

    #[test]
    fn an_unusual_status_survives_the_trip() {
        // The relay passes an upstream status through, and an upstream is free
        // to answer one nothing here has heard of.
        assert_eq!(code_of(499).as_u16(), 499);
        let response: hyper::Response<_> = Reply::empty(418).into();
        assert_eq!(response.status().as_u16(), 418);
    }

    #[test]
    fn a_code_hyper_cannot_send_becomes_a_500_rather_than_a_panic() {
        assert_eq!(code_of(0), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
