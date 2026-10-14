// SPDX-License-Identifier: MPL-2.0

//! Relaying a request to the daemon and its answer back to the browser.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use tokio::sync::mpsc;

use crate::body::{BoxBody, json_response};
use crate::config::Upstream;

/// Largest request body the kiosk will relay.
///
/// The kiosk is a browser front door, not an upload service; anything larger is
/// a mistake or an attack, and the limit is what keeps it out of memory.
pub const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

/// How many response chunks may queue between the upstream reader and hyper.
///
/// Depth is back-pressure: a browser that stops reading parks the reader at
/// this many chunks instead of letting it pull the whole response into memory.
const STREAM_CHANNEL_DEPTH: usize = 16;

/// Headers that describe THIS hop and must not be relayed to the next one.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            // ureq picks its own encoding and decodes it back; forwarding the
            // browser's preference would only invite a double decode.
            | "accept-encoding"
    )
}

/// Relays one request to the upstream daemon and returns its answer.
///
/// The request body is buffered (ureq wants it whole, and [`MAX_REQUEST_BODY`]
/// bounds it); the response body is streamed, so a long-lived upstream answer
/// — a streamed tab, an SSE feed — reaches the browser as it is produced rather
/// than after it ends.
pub async fn proxy(req: Request<Incoming>, upstream: &Upstream) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();

    // Bounded, buffered read: ureq wants the whole body up front anyway, and the
    // limit is what keeps a big upload from being accumulated in memory.
    let body = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) if err.is::<http_body_util::LengthLimitError>() => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &serde_json::json!({ "error": "request_body_too_large" }),
            );
        }
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "unreadable_request_body" }),
            );
        }
    };

    let target = format!(
        "{}{}{}",
        upstream.base,
        parts.uri.path(),
        parts.uri.query().map_or_else(String::new, |q| format!("?{q}"))
    );

    let method = parts.method.clone();
    let headers: Vec<(HeaderName, HeaderValue)> = parts
        .headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();

    let agent = upstream.agent.clone();
    // ureq is blocking; running it on the reactor would stall every other
    // connection for the duration of the hop.
    let upstream_resp = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let mut call = http::Request::builder().method(method).uri(&target);
        for (name, value) in &headers {
            call = call.header(name, value);
        }
        let request = call
            .body(body.to_vec())
            .map_err(|err| format!("bad upstream request: {err}"))?;
        agent.run(request).map_err(|err| format!("{err}"))
    })
    .await
    .map_err(|err| format!("upstream task: {err}"))
    .and_then(std::convert::identity);

    let (parts, body) = match upstream_resp {
        Ok(resp) => resp.into_parts(),
        Err(detail) => {
            // The reason may name an internal host: log it, tell the browser
            // only that the upstream is the one that failed.
            eprintln!("upstream hop failed: {detail}");
            return json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({ "error": "upstream_unavailable" }),
            );
        }
    };

    // Hand the upstream body to hyper as it arrives instead of buffering it:
    // that is what keeps a chunked/streamed response flowing. The blocking
    // reader owns the connection, so it runs on a thread of its own; it ends on
    // EOF, on any read error, on its idle timeout, or when `tx` fails because
    // the browser went away.
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(STREAM_CHANNEL_DEPTH);
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut reader = body.into_reader();
        let mut buf = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(Frame::data(Bytes::copy_from_slice(&buf[..n]))))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    break;
                }
            }
        }
    });

    let mut builder = Response::builder().status(parts.status);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(StreamBody::new(tokio_stream_of(rx)).boxed())
        .expect("proxied headers came from a real response")
}

/// Adapt the chunk channel into something [`StreamBody`] accepts.
fn tokio_stream_of(
    mut rx: mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
