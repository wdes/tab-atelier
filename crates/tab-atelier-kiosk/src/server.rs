// SPDX-License-Identifier: MPL-2.0

//! The HTTP server: bind, accept, dispatch.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::route;

/// Starts the HTTP server.
///
/// # Errors
/// Returns an error if the address cannot be bound or the server fails.
pub async fn serve(listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("bind {listen}: {error}"))?;
    serve_on(listener).await
}

/// Serves on an already-bound listener.
///
/// Split from [`serve`] so a test can bind port `0` and learn the real address
/// before the loop starts, rather than guessing a free port and racing whatever
/// else is on the machine.
///
/// # Errors
/// Returns an error if accepting a connection fails.
pub async fn serve_on(listener: TcpListener) -> Result<(), String> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|error| format!("accept connection: {error}"))?;
        tokio::spawn(async move {
            let service = service_fn(|request: Request<hyper::body::Incoming>| async move {
                Ok::<_, std::convert::Infallible>(answer(&request))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// The response for one request.
///
/// Kept as a free function so a test can drive it without a socket; the socket
/// test beside it proves this is what a client actually meets.
fn answer(request: &Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    if request.uri().path() == "/health" {
        return Response::new(Full::new(Bytes::from_static(b"ok")));
    }
    route::respond(&route::route(request.uri().path()))
}

#[cfg(test)]
mod tests {
    use super::serve_on;
    use crate::route::{Route, route};

    /// Sends one raw request and returns `(status, headers+body)`.
    ///
    /// `Connection: close` so `read_to_end` sees the end of the response: hyper
    /// keeps the socket open for a second request otherwise, and the read would
    /// hang rather than fail.
    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read");
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// A request over a real socket gets the real answer.
    ///
    /// The unit test above proves the table is right; this proves the table is
    /// what a client actually meets. The disjunction matters here: `respond`,
    /// `route` and the connection loop are all in the path, and a mistake in any
    /// of them — a status line built wrong, a content type dropped — is invisible
    /// to a test that only calls the table.
    #[tokio::test]
    async fn the_server_answers_requests_over_a_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = serve_on(listener).await;
        });

        let page = get(addr, "/").await;
        assert!(page.starts_with("HTTP/1.1 200"), "page did not 200: {page:.80}");
        assert!(page.contains("text/html"), "page lost its content type");
        assert!(
            page.to_ascii_lowercase().contains("<!doctype html"),
            "page did not carry the html body"
        );

        let script = get(addr, "/assets/kiosk.js").await;
        assert!(script.starts_with("HTTP/1.1 200"), "script did not 200");
        assert!(script.contains("text/javascript"), "script lost its content type");
        assert!(script.len() > 1000, "script body looks truncated");

        let health = get(addr, "/health").await;
        assert!(health.starts_with("HTTP/1.1 200"), "health did not 200");
        assert!(health.ends_with("ok"), "health body was not `ok`: {health:.60}");

        // The one that matters most: an unhandled path must answer 404, not the
        // page. A Kiosk that serves its own HTML for the daemon's routes looks
        // like it works while answering nothing.
        let missing = get(addr, "/decisions").await;
        assert!(
            missing.starts_with("HTTP/1.1 404"),
            "an unhandled path did not 404: {missing:.80}"
        );
    }

    /// The router and the socket agree on every path the table knows.
    ///
    /// `route` is the single source of truth; this checks the two never drift,
    /// catching a path wired into one and not the other.
    #[test]
    fn the_table_and_the_wire_agree() {
        assert!(matches!(route("/"), Route::Asset(..)));
        assert!(matches!(route("/nope"), Route::NotFound));
    }
}
