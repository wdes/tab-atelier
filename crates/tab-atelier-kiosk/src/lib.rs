// SPDX-License-Identifier: MPL-2.0

//! The Kiosk web GUI: its embedded assets, and the routes that serve them.
//!
//! The Kiosk logic is still to come — the panes (decision, report, intent) are
//! the daemon's routes today, and this crate does not answer them yet. What is
//! here is the interface itself: the HTML, CSS and JavaScript are compiled into
//! the binary, so a package cannot be installed without the files it needs.
//!
//! `include_str!` rather than reading from disk at run time, and that is a
//! deployment decision rather than a performance one: a `.deb` that carries a
//! binary and a directory of assets can be installed with one of them missing,
//! and the failure shows up as a blank page in a browser nobody is watching.
//! Compiled in, the two cannot come apart.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// The interface, compiled in. See the module documentation for why.
mod assets {
    pub const HTML: &str = include_str!("../assets/kiosk.html");
    pub const CSS: &str = include_str!("../assets/kiosk.css");
    pub const JS: &str = include_str!("../assets/kiosk.js");
}

/// What a request resolves to before any handler runs it.
enum Route {
    /// Serve these bytes with this content type.
    Asset(&'static str, &'static str),
    /// Not ours. The Kiosk panes are the daemon's routes, and once this crate
    /// answers requests on their behalf this arm becomes the reverse proxy the
    /// dashboard carries today.
    NotFound,
}

/// Which local asset a path names, if any.
///
/// `/` and `/kiosk` are the same page: the former is what a browser asks for,
/// the latter is what a link can be written against without depending on being
/// the root of a host.
fn route(path: &str) -> Route {
    match path {
        "/" | "/kiosk" | "/assets/kiosk.html" => Route::Asset(assets::HTML, "text/html; charset=utf-8"),
        "/assets/kiosk.css" => Route::Asset(assets::CSS, "text/css; charset=utf-8"),
        "/assets/kiosk.js" => Route::Asset(assets::JS, "text/javascript; charset=utf-8"),
        _ => Route::NotFound,
    }
}

fn respond(route: &Route) -> Response<Full<Bytes>> {
    match route {
        Route::Asset(body, content_type) => {
            let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, content_type.parse().expect("static header value"));
            response
        }
        Route::NotFound => {
            let mut response = Response::new(Full::new(Bytes::from_static(b"not found")));
            *response.status_mut() = StatusCode::NOT_FOUND;
            response
        }
    }
}

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
                let response = if request.uri().path() == "/health" {
                    Response::new(Full::new(Bytes::from_static(b"ok")))
                } else {
                    respond(&route(request.uri().path()))
                };
                Ok::<_, std::convert::Infallible>(response)
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Route, assets, route, serve_on};

    /// The page and its two companions are served, and nothing else is.
    ///
    /// The point is the second half: an unknown path must not fall through to
    /// the page, because a Kiosk that answers the harness routes with its own
    /// HTML would look like it works while answering nothing.
    #[test]
    fn only_the_interface_paths_are_served() {
        for path in [
            "/",
            "/kiosk",
            "/assets/kiosk.html",
            "/assets/kiosk.css",
            "/assets/kiosk.js",
        ] {
            assert!(matches!(route(path), Route::Asset(..)), "{path} should serve an asset");
        }
        for path in ["/decisions", "/reports", "/intent", "/nope", "/assets/"] {
            assert!(
                matches!(route(path), Route::NotFound),
                "{path} should not be served by this crate yet"
            );
        }
    }

    /// The assets are the compiled-in ones, not empty placeholders.
    ///
    /// Guards the failure `include_str!` cannot catch on its own: an empty file
    /// compiles fine and serves a blank page.
    #[test]
    fn the_assets_have_content() {
        // Case-insensitive: the file writes `<!doctype html>`, and which case a
        // doctype uses is not something this test has an opinion about.
        assert!(
            assets::HTML.to_ascii_lowercase().contains("<!doctype html"),
            "html looks empty"
        );
        assert!(assets::CSS.len() > 1000, "css looks empty");
        assert!(assets::JS.len() > 1000, "js looks empty");
    }

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
}
