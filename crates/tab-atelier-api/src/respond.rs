// SPDX-License-Identifier: MPL-2.0

//! How a [`Reply`] becomes a Rocket response.
//!
//! With this in place a handler never names a Rocket type. It returns `Reply` —
//! status, headers, body — and the framework adapts it, which is what lets the
//! same handler be driven by Rocket or by a plain hyper loop.
//!
//! Two shapes have to be handled. A buffered body is copied straight into the
//! response. A *streamed* body is where the work is: a producer makes chunks as
//! they arrive, Rocket wants an `AsyncRead`, and the two are different
//! interfaces for the same idea. Rather than buffer the stream — which would
//! hold a whole answer in memory and make SSE look like a hang — [`BodyReader`]
//! bridges them, pulling one frame at a time and handing out its bytes as they
//! are asked for.
//!
//! The chunked transfer encoding and the absence of a `Content-Length` are
//! Rocket's business, not ours: it sees a body of unknown length and does the
//! right thing.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::Body as HttpBody;
use rocket::http::{Header, Status};
use rocket::request::Request;
use rocket::response::Responder;
use rocket::{Response, response};

use crate::marshal::{Reply, ReplyBody};
use crate::resource::ProblemResource;
use crate::schemars::JsonSchema;
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::okapi::Map;
use rocket_okapi::okapi::openapi3::{MediaType, Response as DocResponse, Responses};
use rocket_okapi::response::OpenApiResponderInner;
use serde::Serialize;

/// Documenting a `Reply` — as a body the handler shaped at run time.
///
/// A `Reply` is deliberately untyped: the relay passes an upstream answer
/// through with whatever status and body it came with, and the asset routes
/// carry a file they read. There is no `T` for okapi to describe, so a route
/// returning one is documented with no schema and marked `x-dynamic-body`.
///
/// The marker is a ratchet, not a shrug: a route that could carry a typed
/// resource and still returns a bare `Reply` shows up as a `x-dynamic-body`
/// count a test can assert on, so the number can only be made to fall. Without
/// it, "every response is documented" would be quietly false for the routes
/// that matter most.
impl OpenApiResponderInner for Reply {
    fn responses(_gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut extensions = Map::new();
        extensions.insert("x-dynamic-body".to_owned(), serde_json::Value::Bool(true));
        let response = DocResponse {
            description: "a body shaped at run time; see the handler".to_owned(),
            extensions,
            ..DocResponse::default()
        };
        let mut responses = Responses::default();
        responses.responses.insert("200".to_owned(), response.into());
        Ok(responses)
    }
}

impl<'r> Responder<'r, 'static> for Reply {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let mut builder = Response::build();
        builder.status(status_of(self.status));
        // Whether the reply already says what it is. A `Reply` an asset route
        // built carries the type it worked out from the file extension, and this
        // adapter used to override it with `application/json` on every buffered
        // body — which, combined with the `nosniff` header, made the browser
        // refuse to execute the UI's own scripts and styles and rendered the
        // page blank. A reply that knows what it is holding has to win.
        let mut typed = false;
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("content-type") {
                typed = true;
            }
            // Built from its two public fields rather than through a `From`
            // impl: `Header` has no conversion from an owned pair, and every
            // name here is a `&'static str` from the reply layer, so there is
            // nothing that can fail.
            builder.header(Header {
                name: (*name).into(),
                value: value.clone().into(),
            });
        }
        // The default is applied only when the reply did not decide for itself.
        // JSON is the right guess for this API, but it is a guess.
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

/// A typed resource, answered as JSON — and documented by its type.
///
/// Where [`Reply`] carries bytes the handler already shaped, `Doc` carries the
/// *value*, so okapi documents the response with the value's schema instead of
/// the `x-dynamic-body` marker. A handler that answers a named resource returns
/// one of these, and each conversion lowers the untyped count the ratchet holds.
///
/// The status is a **type parameter**, not a field, because okapi documents a
/// responder by its *type*: `Doc<T, 201>` and `Doc<T>` are different types, and
/// only a type-level code can be written into the document. The default is
/// `200`; a creation says `Doc<NewThing, 201>` and a deletion `Doc<(), 204>`,
/// and the run-time status follows the type so the two can never disagree.
///
/// Named `Doc` and not `Json` because `Json` is already this crate's request
/// body; one name for a request and a response would be a trap.
#[derive(Debug)]
pub struct Doc<T, const CODE: u16 = 200> {
    /// The status, as the plain number [`Reply`] carries — `CODE` is written
    /// into the type so okapi can document it, and mirrored in this field so a
    /// caller reads a `Doc` and a `Reply` the same way.
    pub status: u16,
    value: T,
}

impl<T, const CODE: u16> Doc<T, CODE> {
    /// The resource, answered with this type's status.
    #[must_use]
    pub const fn ok(value: T) -> Self {
        Self { status: CODE, value }
    }
}

impl<T: Serialize, const CODE: u16> Responder<'_, 'static> for Doc<T, CODE> {
    fn respond_to(self, _req: &Request<'_>) -> response::Result<'static> {
        // A `Serialize` that fails is a bug in the resource, not a bad request,
        // so it answers 500 rather than panicking inside a request.
        let body = serde_json::to_vec(&self.value).map_err(|_| Status::InternalServerError)?;
        let mut builder = Response::build();
        builder.status(Status::new(self.status));
        builder.header(Header::new("content-type", "application/json"));
        builder.sized_body(body.len(), std::io::Cursor::new(body));
        builder.ok()
    }
}

impl<T: JsonSchema, const CODE: u16> OpenApiResponderInner for Doc<T, CODE> {
    fn responses(g: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let schema = g.json_schema::<T>();
        let mut responses = Responses::default();
        // Documented under this type's own code — 201 for a creation, 200 for a
        // read — because the code is part of the type the route returns.
        rocket_okapi::util::add_schema_response(&mut responses, CODE, "application/json", schema)?;
        Ok(responses)
    }
}

/// A typed refusal: the `{"error": …}` body, documented as `default`.
///
/// The counterpart of [`Doc`] for the failure side. A refusal's status is a
/// run-time choice — the same handler answers 400 for a bad field, 404 for a
/// missing one, 409 for a clash — so no `const CODE` can name it, and a bare
/// [`Reply`] would leave the `x-dynamic-body` marker where a real schema
/// belongs. `Failure` documents `ProblemResource` under the document's
/// `default` response — the honest description of "this may fail, and here is
/// the shape when it does" — while carrying the real status at run time.
///
/// A route that may refuse answers `Result<Doc<T>, Failure>`, and now both arms
/// are typed.
#[derive(Debug)]
pub struct Failure {
    /// The status it will answer with.
    pub status: u16,
    problem: ProblemResource,
}

impl Failure {
    /// A refusal with a status and a reason.
    ///
    /// Takes anything string-like, as the handlers pass both literals and
    /// `format!`-built messages.
    #[must_use]
    pub fn problem(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            problem: ProblemResource { error: message.into() },
        }
    }

    /// The problem body it carries — for a test that reads the reason without
    /// dispatching the whole request.
    #[must_use]
    pub const fn body(&self) -> &ProblemResource {
        &self.problem
    }
}

impl Responder<'_, 'static> for Failure {
    fn respond_to(self, _req: &Request<'_>) -> response::Result<'static> {
        let body = serde_json::to_vec(&self.problem).map_err(|_| Status::InternalServerError)?;
        let mut builder = Response::build();
        builder.status(Status::new(self.status));
        builder.header(Header::new("content-type", "application/json"));
        builder.sized_body(body.len(), std::io::Cursor::new(body));
        builder.ok()
    }
}

impl OpenApiResponderInner for Failure {
    fn responses(g: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let schema = g.json_schema::<ProblemResource>();
        let mut content = Map::new();
        content.insert(
            "application/json".to_owned(),
            MediaType {
                schema: Some(schema),
                ..MediaType::default()
            },
        );
        let response = DocResponse {
            description: "a refusal; `error` is the reason".to_owned(),
            content,
            ..DocResponse::default()
        };
        Ok(Responses {
            default: Some(response.into()),
            ..Responses::default()
        })
    }
}

/// A Rocket status from the numeric one a reply carries.
///
/// A [`Reply`] speaks `u16` so it stays free of the web framework. This is the
/// boundary, so this is where the translation belongs.
#[must_use]
pub const fn status_of(code: u16) -> Status {
    Status::new(code)
}

/// A body that is still being produced, as something Rocket can read.
///
/// Adapter rather than a buffer. The producer arrives as a sequence of frames,
/// Rocket's streaming responder wants `poll_read`, and the difference is only
/// about who drives whom: this pulls the next frame when the reader runs out and
/// then serves bytes out of it, so a chunk leaves for the client as soon as it
/// arrives.
///
/// The whole point is that memory stays bounded: at most one frame plus what the
/// reader has not consumed yet, whatever the length of the answer.
pub struct BodyReader<B> {
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
                let chunk = self.held.split_to(take);
                buf.put_slice(&chunk);
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
                    // this through Rocket's streaming responder except by ending
                    // the body, and the producer has already reported the
                    // failure where the client can see it.
                    self.done = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BodyReader, status_of};
    use bytes::Bytes;
    use http_body::Body as HttpBody;
    use rocket::http::Status;
    use std::convert::Infallible;

    type BoxedBody = Box<dyn HttpBody<Data = Bytes, Error = Infallible> + Unpin + Send>;

    fn reader_of(frames: &[Bytes]) -> BodyReader<BoxedBody> {
        let body = http_body_util::Full::new(Bytes::from(frames.concat()));
        BodyReader::new(Box::new(body) as Box<_>)
    }

    fn read_all(mut reader: BodyReader<BoxedBody>) -> Vec<u8> {
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            use tokio::io::AsyncReadExt as _;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("read");
            out
        })
    }

    #[test]
    fn a_numbered_status_becomes_the_matching_rocket_status() {
        assert_eq!(status_of(200), Status::Ok);
        assert_eq!(status_of(404), Status::NotFound);
        assert_eq!(status_of(503), Status::ServiceUnavailable);
    }

    #[test]
    fn an_unusual_status_is_still_expressible() {
        // A relay passes an upstream code through, and an upstream is free to
        // invent one.
        assert_eq!(status_of(499).code, 499);
        assert_eq!(status_of(102).code, 102);
    }

    #[test]
    fn the_adapter_hands_back_every_byte_it_was_given() {
        let payload = Bytes::from_static(b"data: {\"x\":1}\n\n");
        assert_eq!(read_all(reader_of(std::slice::from_ref(&payload))), payload.to_vec());
    }

    #[test]
    fn an_empty_body_reads_as_no_bytes_and_then_ends() {
        assert!(read_all(reader_of(&[])).is_empty());
    }

    #[test]
    fn a_body_that_arrives_in_frames_is_reassembled() {
        let body = http_body_util::StreamBody::new(futures_util::stream::iter([
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"one"))),
            Ok(http_body::Frame::data(Bytes::from_static(b"two"))),
            Ok(http_body::Frame::data(Bytes::from_static(b"three"))),
        ]));
        let reader: BodyReader<BoxedBody> = BodyReader::new(Box::new(body));
        assert_eq!(read_all(reader), b"onetwothree".to_vec());
    }
}
