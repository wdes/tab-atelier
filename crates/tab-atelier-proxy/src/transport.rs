// SPDX-License-Identifier: MPL-2.0

//! One request type and one reply type, for every handler.
//!
//! The handlers used to take `hyper::Request<Incoming>` and hand back
//! `hyper::Response<Body>`, which welded the API to hyper: a second server
//! could only be added by rewriting every handler. This module is the seam.
//! A handler sees an [`InReq`] — method, path, headers, and a body already read
//! into memory — and answers with a [`Reply`]. Each transport adapts at its own
//! edge: [`Reply::into_hyper`] here, and a Rocket `Responder` beside it.
//!
//! The body is read eagerly because every handler wants it that way: the relay
//! shapes the JSON, and the admin API parses it. Reading it once at the edge
//! keeps that decision in one place instead of at each call site.

use std::net::IpAddr;

use bytes::Bytes;
use hyper::http::HeaderMap;

/// The boxed body a streamed reply carries.
///
/// Boxed because the concrete type is the relay's — an upstream response body
/// — and naming it here would put the relay's transport choices in the
/// transport layer, which is meant to be free of them.
pub type Body = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

/// A request, with its body already in memory.
#[derive(Debug)]
pub struct InReq {
    /// Hyper's `Method`, which the handlers already compare against
    /// (`Method::GET`, `Method::POST`). Rocket's method type is a different
    /// `http` major, so its edge parses the wire spelling into this one.
    pub method: hyper::Method,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// The immediate peer, for the audit trail. Loopback when the transport
    /// cannot say, which is also what a same-host caller looks like.
    pub peer: IpAddr,
}

impl InReq {
    /// The first value of `name`, trimmed, or `None` if absent or not text.
    ///
    /// Invalid UTF-8 in a header is treated as absent rather than lossy-decoded:
    /// a credential with a replacement character in it is not a credential, and
    /// passing it on would produce a confusing upstream error instead of a
    /// clean rejection here.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
    }
}

/// What a handler answers.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// In wire order. A `Vec` rather than a `HeaderMap` so a transport that
    /// needs repeated headers (Rocket's `Header` is one name/value pair, and
    /// `set_raw` overwrites) can emit every one of them.
    pub headers: Vec<(&'static str, String)>,
    pub body: ReplyBody,
}

/// The two shapes a reply body can take.
pub enum ReplyBody {
    /// A complete body, already in memory.
    Bytes(Bytes),
    /// A body still being produced. The relay uses this: chunks are handed over
    /// as they arrive upstream, so a long model answer is never held whole.
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
/// The admin API is same-origin only, so no CORS headers: a page on another
/// origin cannot read a response even if it can send a request.
#[must_use]
pub fn json(status: u16, body: &str) -> Reply {
    let mut reply = Reply::bytes(status, Bytes::from(body.to_owned()));
    reply.headers.push(("content-type", "application/json".to_owned()));
    reply.headers.push(("cache-control", "no-store".to_owned()));
    reply
}

/// A JSON reply from a value rather than from text.
///
/// This is how a resource is returned: named structs crossing into the
/// handler, serialized once at the edge. Callers never build a string, so a
/// field cannot appear here and be missing from the `OpenAPI` document.
///
/// [`json`] stays for the handful of responses that are a literal or a
/// forwarded upstream body, which have no Rust type to derive a schema from.
///
/// # Errors are the caller's
///
/// Serializing a struct of plain fields cannot fail, so a failure here means a
/// custom `Serialize` did; rather than panic in a request, that yields a 500
/// carrying the reason.
#[must_use]
pub fn json_of<T: serde::Serialize>(status: u16, value: &T) -> Reply {
    match serde_json::to_string(value) {
        Ok(body) => json(status, &body),
        Err(e) => json(500, &format!(r#"{{"error":"could not serialize: {e}"}}"#)),
    }
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
    use super::*;

    #[test]
    fn a_json_reply_is_not_cached() {
        // The admin API answers questions about credentials and quota. A
        // cached "no credentials set" would outlive the credential.
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
        // A transport that stores headers in a map would keep only the last of
        // a repeated name, which is why this is a Vec.
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
