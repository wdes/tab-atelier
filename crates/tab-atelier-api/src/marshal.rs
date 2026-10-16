// SPDX-License-Identifier: MPL-2.0

//! The reply every handler answers with.
//!
//! A handler used to hand back `http::Response<Body>`, which welds an API to one
//! transport. It answers with a [`Reply`] instead, and each transport adapts at
//! its own edge — a Rocket `Responder` in [`crate::respond`], a hyper response
//! in an app that keeps hyper. This module is the transport-neutral half and
//! names no server type, so it is the same reply whichever server is underneath.
//!
//! The body is complete in memory unless it is a [`ReplyBody::Stream`]: the
//! admin surface serializes a struct, and the relay forwards an upstream body —
//! and a streamed one is handed on as it arrives, never held whole.

use bytes::Bytes;

/// The boxed body a streamed reply carries.
///
/// Boxed because the concrete type belongs to whoever streams — an upstream
/// response body, say — and naming it here would put that transport's choices in
/// the reply type, which is meant to be free of them.
pub type Body = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

/// What a handler answers.
#[derive(Debug)]
pub struct Reply {
    /// The status, as a number rather than a transport's own status type.
    pub status: u16,
    /// In wire order. A `Vec` rather than a `HeaderMap` so a transport that
    /// needs repeated headers (Rocket's `Header` is one name/value pair, and
    /// `set_raw` overwrites) can emit every one of them. Do NOT "tidy" this into
    /// a map: it would silently keep only the last of a repeated name.
    pub headers: Vec<(&'static str, String)>,
    /// The body.
    pub body: ReplyBody,
}

/// The two shapes a reply body can take.
pub enum ReplyBody {
    /// A complete body, already in memory.
    Bytes(Bytes),
    /// A body still being produced. Chunks are handed on as they arrive, so a
    /// long answer is never held whole.
    Stream(Body),
}

impl std::fmt::Debug for ReplyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Self::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// A plain-text reply.
#[must_use]
pub fn text(status: u16, msg: &str) -> Reply {
    let mut reply = Reply::bytes(status, Bytes::from(msg.to_owned()));
    reply
        .headers
        .push(("content-type", "text/plain; charset=utf-8".to_owned()));
    reply
}

/// A JSON reply, from an already-serialized body.
///
/// Same-origin only, so no CORS headers: a page on another origin cannot read a
/// response even if it can send a request.
#[must_use]
pub fn json(status: u16, body: &str) -> Reply {
    let mut reply = Reply::bytes(status, Bytes::from(body.to_owned()));
    reply.headers.push(("content-type", "application/json".to_owned()));
    reply.headers.push(("cache-control", "no-store".to_owned()));
    reply
}

/// A JSON reply from a value rather than from text.
///
/// This is how a resource is returned: a named struct crosses into the handler
/// and is serialized once at the edge, so a field cannot appear here and be
/// missing from the API document.
///
/// [`json`] stays for the handful of responses that are a literal or a forwarded
/// body, which have no Rust type to derive a schema from.
///
/// # Errors are the caller's
///
/// Serializing a struct of plain fields cannot fail, so a failure here means a
/// custom `Serialize` did; rather than panic in a request, that yields a 500
/// carrying the reason.
#[must_use]
pub fn json_of<T: serde::Serialize>(status: u16, value: &T) -> Reply {
    serde_json::to_string(value).map_or_else(
        |e| json(500, &format!(r#"{{"error":"could not serialize: {e}"}}"#)),
        |body| json(status, &body),
    )
}

impl Reply {
    /// An empty reply with no content type, for a transport to fill in.
    #[must_use]
    pub const fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ReplyBody::Bytes(Bytes::new()),
        }
    }

    /// A reply with an in-memory body and no headers set.
    #[must_use]
    pub const fn bytes(status: u16, body: Bytes) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ReplyBody::Bytes(body),
        }
    }

    /// A reply whose body is still arriving.
    #[must_use]
    pub const fn stream(status: u16, body: Body) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ReplyBody::Stream(body),
        }
    }

    /// Add a header, keeping any that were set before it.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Reply, ReplyBody, json, text};

    #[test]
    fn a_json_reply_is_not_cached() {
        // The API answers questions about credentials and quota. A cached "no
        // credentials set" would outlive the credential.
        let reply = json(200, "{}");
        assert!(
            reply
                .headers
                .iter()
                .any(|(n, v)| *n == "cache-control" && v == "no-store")
        );
    }

    #[test]
    fn headers_are_kept_in_order_and_all_of_them() {
        // A transport that stores headers in a map would keep only the last of a
        // repeated name, which is why this is a Vec.
        let reply = text(200, "hi")
            .with_header("x-a", "1".to_owned())
            .with_header("x-a", "2".to_owned());
        let seen: Vec<&str> = reply
            .headers
            .iter()
            .filter(|(n, _)| *n == "x-a")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(seen, ["1", "2"]);
    }

    #[test]
    fn a_bodyless_reply_still_carries_its_status() {
        // 204 with a body is a protocol error, so both the status and the
        // emptiness are part of what this constructs.
        let reply = Reply::empty(204);
        assert_eq!(reply.status, 204);
        assert!(matches!(reply.body, ReplyBody::Bytes(ref b) if b.is_empty()));
    }
}
