// SPDX-License-Identifier: MPL-2.0

//! Conditional GETs: one `ETag` per representation, and a 304 that costs no
//! compression.
//!
//! A live fleet is polled. The same page comes back every couple of seconds and
//! is almost always unchanged, so the interesting work happens before the body
//! is encoded: compare the tag the client holds against the tag we would send,
//! and stop.
//!
//! # One tag per representation
//!
//! The gzipped and identity bodies are different bytes. Sharing a tag between
//! them lets a shared cache hand a stored gzip response to a client that never
//! advertised it, and lets a client revalidating an identity copy be told 304
//! for a gzip it does not have. So the compressed representation gets its own
//! tag (`…-gz`) and `Vary: Accept-Encoding` tells any cache to key on the
//! encoding too.
//!
//! # The compressor is not run to answer 304
//!
//! Whether a body *will* be compressed is decided from its length and the
//! client's `Accept-Encoding`, before any encoder is built — a conditional
//! request that is about to be answered with 304 must not pay for compression
//! it will throw away. [`gzip`] then has to agree with that prediction, so the
//! prediction is a function ([`will_gzip`]) rather than a repeated expression.

use bytes::Bytes;

use crate::marshal::Reply;

/// Bodies under this size are not compressed: the gzip header and the CPU cost
/// more than they save on a few kilobytes.
pub const GZIP_MIN_BODY: usize = 4096;

/// What this request said about caching.
#[derive(Debug, Clone, Copy, Default)]
pub struct Conditional<'a> {
    /// The client advertised gzip in `Accept-Encoding`.
    pub accept_gzip: bool,
    /// The bare tag from `If-None-Match`, with the quotes already stripped.
    /// `"a", "b"` or `*` are legal on the wire and are the caller's to
    /// interpret; this crate compares one tag.
    pub if_none_match: Option<&'a str>,
}

impl Conditional<'_> {
    /// A request that said nothing about caching.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            accept_gzip: false,
            if_none_match: None,
        }
    }
}

/// Whether `body` is worth compressing for this request.
///
/// The one place the rule lives, so the pre-check that decides a 304 and the
/// encoder that follows cannot disagree — if they did, a client would be told
/// 304 for a representation it never received.
#[must_use]
pub const fn will_gzip(len: usize, accept_gzip: bool) -> bool {
    accept_gzip && len >= GZIP_MIN_BODY
}

/// Gzip `body`, or `None` to send it as-is.
///
/// Level 1 rather than the default 6: these are live endpoints re-compressed on
/// every response, and terminal text and JSON compress nearly as well at level
/// 1 for a fraction of the CPU. The WebSocket writer made the same call.
#[must_use]
pub fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write as _;

    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 4), flate2::Compression::fast());
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

/// A body that may be cached, answered conditionally.
///
/// `etag` is the caller's tag for the *identity* representation — any stable
/// string derived from `body` (a content hash, a version counter). This function
/// adds the `-gz` suffix itself, so callers never see the distinction.
///
/// Answers 304 when the client already holds this representation and the status
/// is 200. A 304 carries no body, so a caller that only wants to save the
/// transfer keeps it. Any other status is answered in full: a `201` or a `404`
/// is news, and a conditional must not swallow it.
#[must_use]
pub fn cached(status: u16, content_type: &str, body: Bytes, etag: &str, client: Conditional<'_>) -> Reply {
    // Predicted, not measured — and it is what decides the 304, so this costs
    // no compression on the common path. `gzip` below reuses the prediction, so
    // the tag and the bytes it describes cannot disagree.
    let want_gzip = will_gzip(body.len(), client.accept_gzip);
    let tag = if want_gzip {
        format!("{etag}-gz")
    } else {
        etag.to_owned()
    };
    if status == 200 && client.if_none_match == Some(tag.as_str()) {
        return Reply::empty(304)
            .with_header("etag", format!("\"{tag}\""))
            .with_header("vary", "Accept-Encoding".to_owned());
    }
    // The 304 above already returned, so what reaches here is either not
    // conditional or conditional on a different representation — encode now.
    //
    // A failed encode falls back to the identity body and the identity tag,
    // never to the gzip tag: announcing `…-gz` over an uncompressed body would
    // make a cache store a lie under that name.
    let (body, tag) = want_gzip
        .then(|| gzip(&body))
        .flatten()
        .map_or_else(|| (body, etag.to_owned()), |encoded| (Bytes::from(encoded), tag));
    let mut reply = Reply::bytes(status, body)
        .with_header("content-type", content_type.to_owned())
        .with_header("etag", format!("\"{tag}\""));
    if tag.ends_with("-gz") {
        reply = reply.with_header("content-encoding", "gzip".to_owned());
    }
    reply.with_header("vary", "Accept-Encoding".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Conditional, GZIP_MIN_BODY, cached, gzip, will_gzip};
    use bytes::Bytes;

    fn big() -> Bytes {
        Bytes::from("the same line of terminal text over and over\n".repeat(200))
    }

    #[test]
    fn a_conditional_hit_on_the_identity_representation_is_a_304() {
        let reply = cached(
            200,
            "application/json",
            big(),
            "abc",
            Conditional {
                accept_gzip: false,
                if_none_match: Some("abc"),
            },
        );
        assert_eq!(reply.status, 304);
        assert!(reply.headers.contains(&("etag", "\"abc\"".to_owned())));
    }

    #[test]
    fn a_client_holding_the_identity_cannot_be_told_304_for_the_gzip_one() {
        // The two representations have different bytes, so a shared tag would
        // let a cache hand out a gzip the client never asked for.
        let reply = cached(
            200,
            "application/json",
            big(),
            "abc",
            Conditional {
                accept_gzip: true,
                if_none_match: Some("abc"),
            },
        );
        assert_eq!(reply.status, 200, "the tag for the gzip form is `abc-gz`");
        assert!(reply.headers.contains(&("etag", "\"abc-gz\"".to_owned())));
        assert!(reply.headers.contains(&("content-encoding", "gzip".to_owned())));
    }

    #[test]
    fn a_gzip_client_holding_the_gzip_form_is_told_304() {
        let reply = cached(
            200,
            "application/json",
            big(),
            "abc",
            Conditional {
                accept_gzip: true,
                if_none_match: Some("abc-gz"),
            },
        );
        assert_eq!(reply.status, 304);
    }

    #[test]
    fn every_cached_reply_varies_on_the_encoding() {
        // Without this a cache may key on the URL alone and serve whichever
        // representation it stored first.
        for accept_gzip in [false, true] {
            let reply = cached(
                200,
                "text/plain",
                big(),
                "abc",
                Conditional {
                    accept_gzip,
                    if_none_match: None,
                },
            );
            assert!(
                reply.headers.contains(&("vary", "Accept-Encoding".to_owned())),
                "accept_gzip={accept_gzip}"
            );
        }
    }

    #[test]
    fn a_small_body_is_not_compressed_even_when_offered() {
        let reply = cached(
            200,
            "text/plain",
            Bytes::from_static(b"tiny"),
            "abc",
            Conditional {
                accept_gzip: true,
                if_none_match: None,
            },
        );
        assert_eq!(reply.status, 200);
        assert!(reply.headers.contains(&("etag", "\"abc\"".to_owned())));
        assert!(!reply.headers.iter().any(|(n, _)| *n == "content-encoding"));
    }

    #[test]
    fn a_non_200_is_never_turned_into_a_304() {
        // A 404 with a matching tag is still news: the resource went away.
        let reply = cached(
            404,
            "application/json",
            Bytes::from_static(b"{}"),
            "abc",
            Conditional {
                accept_gzip: false,
                if_none_match: Some("abc"),
            },
        );
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn a_compressed_body_round_trips_byte_exact() {
        // The daemon's own test for this lived beside its hand-rolled encoder;
        // the encoder moved here, so the property moves with it. A body that
        // decompresses to something *nearly* right would be worse than no
        // compression at all, and only a round-trip catches that.
        let text = "the same line of terminal text over and over\n".repeat(200);
        let encoded = gzip(text.as_bytes()).expect("big enough to compress");
        assert!(
            encoded.len() < text.len() / 4,
            "repetitive text shrinks a lot: {} -> {}",
            text.len(),
            encoded.len()
        );
        let mut decoder = flate2::read::GzDecoder::new(encoded.as_slice());
        let mut round = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut round).expect("valid gzip");
        assert_eq!(round, text);
    }

    #[test]
    fn the_prediction_and_the_encoder_agree() {
        // They are separate functions; if they disagreed a client could be told
        // 304 for a representation it was never sent.
        assert!(!will_gzip(GZIP_MIN_BODY - 1, true));
        assert!(!will_gzip(GZIP_MIN_BODY, false));
        let body = vec![b'x'; GZIP_MIN_BODY];
        assert!(will_gzip(body.len(), true), "the floor is inclusive");
        assert!(gzip(&body).is_some(), "and the encoder agrees at the floor");
    }
}
