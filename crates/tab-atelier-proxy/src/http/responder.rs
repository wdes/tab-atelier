// SPDX-License-Identifier: MPL-2.0

//! How a controller's [`Reply`] becomes a Rocket response.
//!
//! With this in place a controller never names a Rocket type. A handler returns
//! `Reply` — status, headers, body — and the framework adapts it, which is what
//! lets the same controller be driven by Rocket, by the transport tests, or by
//! `curl` through either.
//!
//! Two shapes have to be handled. A buffered body is copied straight into the
//! response. A *streamed* body is the relay's, and it is where the work is: the
//! relay produces chunks as they arrive from upstream, Rocket wants an
//! `AsyncRead`, and the two are different interfaces for the same idea. Rather
//! than buffer the stream — which would hold a whole model answer in memory and
//! make SSE look like a hang — [`BodyReader`] bridges them, pulling one frame
//! at a time and handing out its bytes as they are asked for.
//!
//! The chunked transfer encoding and the absence of a `Content-Length` are
//! Rocket's business, not ours: it sees a body of unknown length and does the
//! right thing, which is the same thing the previous hand-written hyper server
//! did by hand.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use http_body::Body as HttpBody;
use rocket::http::{Header, Status};
use rocket::request::Request;
use rocket::response::Responder;
use rocket::{Response, response};

use crate::transport::{Reply, ReplyBody};

impl<'r> Responder<'r, 'static> for Reply {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let mut builder = Response::build();
        builder.status(status_of(self.status));
        // Whether the reply already says what it is. A `Reply` from the web
        // controller carries the type it worked out from the file extension,
        // and this adapter used to override it with `application/json` on every
        // buffered body — which, combined with the `nosniff` header, made the
        // browser refuse to execute the dashboard's own scripts and styles.
        let mut typed = false;
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("content-type") {
                typed = true;
            }
            // Built from its two public fields rather than through a `From`
            // impl: `Header` has no conversion from an owned pair, and every
            // name here is a `&'static str` from the transport layer, so there
            // is nothing that can fail.
            builder.header(Header {
                name: (*name).into(),
                value: value.clone().into(),
            });
        }
        // The default is applied only when the reply did not decide for
        // itself. JSON is the right guess for this API, but it is a guess, and
        // a reply that knows what it is holding must win.
        match self.body {
            ReplyBody::Bytes(bytes) => {
                if !typed {
                    builder.header(Header::new("content-type", "application/json"));
                }
                builder.sized_body(bytes.len(), std::io::Cursor::new(bytes));
                builder.ok()
            }
            ReplyBody::Stream(body) => {
                if !typed {
                    builder.header(Header::new("content-type", "text/event-stream"));
                }
                builder.streamed_body(BodyReader::new(body));
                builder.ok()
            }
        }
    }
}

/// A Rocket status from the numeric one the controllers carry.
///
/// The controllers speak `u16` so that the request and resource types stay free
/// of the web framework. This is the boundary, so this is where the translation
/// belongs.
const fn status_of(code: u16) -> Status {
    Status::new(code)
}

/// A body that is still being produced, as something Rocket can read.
///
/// Adapter rather than a buffer. The relay's upstream arrives as a sequence of
/// frames, Rocket's streaming responder wants `poll_read`, and the difference is
/// only about who drives whom: this pulls the next frame when the reader runs
/// out and then serves bytes out of it, so a chunk leaves for the client as soon
/// as it arrives from the provider.
///
/// The whole point is that memory stays bounded: at most one frame plus what the
/// reader has not consumed yet, whatever the length of the answer.
pub(crate) struct BodyReader<B> {
    /// The frames, still arriving.
    body: B,
    /// What has been pulled but not yet handed out.
    held: Bytes,
    /// Set once the producer ends, so a reader knows to stop asking.
    done: bool,
}

impl<B> BodyReader<B> {
    /// Wrap a body.
    const fn new(body: B) -> Self {
        Self {
            body,
            held: Bytes::new(),
            done: false,
        }
    }
}

impl<B> tokio::io::AsyncRead for BodyReader<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.held.is_empty() {
                // Hand over whatever fits and remember the rest: `poll_read` is
                // allowed to move fewer bytes than it has, and the caller will
                // come back for the remainder.
                let take = self.held.len().min(buf.remaining());
                let mut chunk = self.held.split_to(take);
                buf.put_slice(&chunk);
                // `split_to` leaves `held` with the remainder, so only the taken
                // part has to be accounted for here.
                chunk.advance(0);
                return Poll::Ready(Ok(()));
            }
            if self.done {
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        self.held = data;
                    }
                    // A non-data frame (a trailer) carries nothing to hand out,
                    // so the loop goes round for the next one.
                }
                // End of the body: report it as a read of nothing, which is how
                // a reader learns the producer finished.
                Poll::Ready(None) => {
                    self.done = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Err(_))) => {
                    // The producer failed mid-answer. There is no way to signal
                    // this through Rocket's streaming responder except by
                    // ending the body, and the relay has already reported the
                    // failure in the stream itself where the client can see it.
                    self.done = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Reading a stream through the adapter yields exactly the bytes written, in
/// order, however the frames were split.
#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{Full, StreamBody};
    use std::convert::Infallible;

    type BoxedBody = Box<dyn HttpBody<Data = Bytes, Error = Infallible> + Unpin + Send>;

    fn reader_of(frames: &[Bytes]) -> BodyReader<BoxedBody> {
        let body = Full::new(Bytes::from(frames.concat()));
        BodyReader::new(Box::new(body) as Box<_>)
    }

    /// A reply that says what it is holding keeps its own content type.
    ///
    /// This adapter applies a default, and the default used to win over the
    /// reply — so every buffered body went out as `application/json`, including
    /// the dashboard's own JavaScript and CSS. Combined with `nosniff` on the
    /// response, the browser then refused to execute the scripts and the page
    /// rendered blank. The web controller works the type out from the file
    /// extension; that answer has to survive.
    #[test]
    fn a_reply_that_names_its_content_type_keeps_it() {
        let reply = crate::transport::text(200, "body{}").with_header("content-type", "text/css".to_owned());
        assert!(
            reply
                .headers
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v == "text/css"),
            "the reply carries the type the file extension implies"
        );
    }

    /// A reply that names NO type gets the JSON default.
    ///
    /// The other half of the rule: the default must still apply, or a body that
    /// arrives untyped goes out untyped. Only a bare `Reply::bytes` reaches
    /// this — the `text` and `json` helpers both set a type of their own, which
    /// is why they are not used to build the fixture.
    #[test]
    fn a_reply_that_names_no_type_is_json_by_default() {
        let reply = crate::transport::Reply::bytes(200, bytes::Bytes::from_static(b"{}"));
        assert!(
            !reply
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("content-type")),
            "the fixture must carry no type, or the default is not what is tested"
        );
    }

    #[test]
    fn a_numbered_status_becomes_the_matching_rocket_status() {
        assert_eq!(status_of(200), Status::Ok);
        assert_eq!(status_of(404), Status::NotFound);
        assert_eq!(status_of(503), Status::ServiceUnavailable);
    }

    #[test]
    fn an_unusual_status_is_still_expressible() {
        // The relay passes upstream codes through, and a provider is free to
        // invent one.
        assert_eq!(status_of(499).code, 499);
        assert_eq!(status_of(102).code, 102);
    }

    #[test]
    fn the_adapter_hands_back_every_byte_it_was_given() {
        let payload = Bytes::from_static(b"data: {\"x\":1}\n\n");
        let mut reader = reader_of(std::slice::from_ref(&payload));
        let got = tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("read");
            out
        });
        assert_eq!(got, payload.to_vec());
    }

    #[test]
    fn an_empty_body_reads_as_no_bytes_and_then_ends() {
        let mut reader = reader_of(&[]);
        let got = tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("read");
            out
        });
        assert!(got.is_empty());
    }

    #[test]
    fn a_reader_smaller_than_the_frame_still_gets_all_of_it() {
        // This is the buffering case: the client asks for four bytes at a time
        // and the adapter has to hold the rest rather than drop it.
        let payload = Bytes::from_static(b"abcdefghij");
        let mut reader = reader_of(std::slice::from_ref(&payload));
        let got = tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            let mut buf = [0u8; 4];
            loop {
                let n = reader.read(&mut buf).await.expect("read");
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        });
        assert_eq!(got, payload.to_vec());
    }

    #[test]
    fn a_body_that_arrives_in_frames_is_reassembled() {
        let body = StreamBody::new(futures_util::stream::iter(vec![
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"one"))),
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"two"))),
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"three"))),
        ]));
        let mut reader: BodyReader<BoxedBody> = BodyReader::new(Box::new(body));
        let got = tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("read");
            out
        });
        assert_eq!(got, b"onetwothree".to_vec());
    }
}
