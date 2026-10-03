// SPDX-License-Identifier: MPL-2.0

//! Scaffold only: Kiosk routes will arrive when the existing Kiosk PRs are rebased.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Starts the HTTP server with the scaffold health endpoint.
///
/// # Errors
/// Returns an error if the address cannot be bound or the server fails.
pub async fn serve(listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("bind {listen}: {error}"))?;

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
                    let mut response = Response::new(Full::new(Bytes::from_static(b"not found")));
                    *response.status_mut() = StatusCode::NOT_FOUND;
                    response
                };
                Ok::<_, std::convert::Infallible>(response)
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}
