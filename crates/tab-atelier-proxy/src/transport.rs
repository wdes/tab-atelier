// SPDX-License-Identifier: MPL-2.0

//! The request type the handlers take, and the re-export of the reply type.
//!
//! The handlers used to take `http::Request<Incoming>` and hand back
//! `http::Response<Body>`, which welded the API to hyper. This module is the
//! request half of the seam: a handler sees an [`InReq`] — method, path,
//! headers, and a body already read into memory.
//!
//! The reply half has moved to [`tab_atelier_api`], where it is shared with the
//! kiosk: the `Reply`, its constructors and the Rocket `Responder` are one
//! definition both servers write to. They are re-exported here so every
//! `use crate::transport::{Reply, json_of}` in this crate keeps naming one
//! module.
//!
//! The body is read eagerly because every handler wants it that way: the relay
//! shapes the JSON, and the admin API parses it. Reading it once at the edge
//! keeps that decision in one place instead of at each call site.

use std::net::IpAddr;

use bytes::Bytes;
use http::HeaderMap;

pub use tab_atelier_api::marshal::{Body, Reply, ReplyBody, json, json_of, text};

/// A request, with its body already in memory.
#[derive(Debug)]
pub struct InReq {
    /// Hyper's `Method`, which the handlers already compare against
    /// (`Method::GET`, `Method::POST`). Rocket's method type is a different
    /// `http` major, so its edge parses the wire spelling into this one.
    pub method: http::Method,
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
