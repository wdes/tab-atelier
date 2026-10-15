// SPDX-License-Identifier: MPL-2.0

//! Reaching a remote's daemon, with the token that opens it.
//!
//! Every call here runs server-side. The browser authenticates to the Kiosk, and
//! the Kiosk authenticates to each remote — so the fleet's tokens never enter a
//! page, and a cross-site script in the Kiosk origin cannot walk away with the
//! credentials of every server the operator owns.
//!
//! The calls are blocking (ureq); the caller runs each on a blocking thread so
//! the reactor keeps serving other connections while a slow remote answers.

use http::Request;
use serde_json::Value;

use crate::config::HopTimeouts;
use crate::remotes::Remote;

/// Why a remote call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// The remote could not be reached, or the request could not be built.
    Unreachable(String),
}

/// A client for the remotes' daemons.
#[derive(Debug, Clone)]
pub struct Daemon {
    agent: ureq::Agent,
}

impl Daemon {
    /// A client with the given hop deadlines, shared across every remote: one
    /// connection pool, one place the timeouts are set.
    pub fn new(timeouts: HopTimeouts) -> Self {
        let agent = ureq::Agent::config_builder()
            // A 404 from a remote is a 404 for the caller, not a transport error.
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_resolve(timeouts.resolve)
            .timeout_connect(timeouts.connect)
            .timeout_send_request(timeouts.send_request)
            .timeout_send_body(timeouts.send_body)
            .timeout_recv_response(timeouts.recv_response)
            .timeout_recv_body(timeouts.recv_body)
            .build()
            .new_agent();
        Self { agent }
    }

    /// `GET <remote><path>`, returning the status and the body as text.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Unreachable`] when the remote does not answer.
    pub fn get(&self, remote: &Remote, path_and_query: &str) -> Result<(u16, String), DaemonError> {
        self.request(remote, "GET", path_and_query, None)
    }

    /// `POST <remote><path>` with a JSON body, returning the status and body.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Unreachable`] when the remote does not answer.
    pub fn post(&self, remote: &Remote, path: &str, body: &Value) -> Result<(u16, String), DaemonError> {
        let encoded = serde_json::to_vec(body).map_err(|err| DaemonError::Unreachable(err.to_string()))?;
        self.request(remote, "POST", path, Some(encoded))
    }

    fn request(
        &self,
        remote: &Remote,
        method: &str,
        path_and_query: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, String), DaemonError> {
        let url = format!("{}{}", remote.url, path_and_query);
        let mut builder = Request::builder().method(method).uri(&url);
        if !remote.token.is_empty() {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {}", remote.token));
        }
        builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        let request = builder
            .body(body.unwrap_or_default())
            .map_err(|err| DaemonError::Unreachable(err.to_string()))?;

        match self.agent.run(request) {
            Ok(response) => {
                let status = response.status().as_u16();
                let text = response.into_body().read_to_string().unwrap_or_else(|_| String::new());
                Ok((status, text))
            }
            Err(err) => Err(DaemonError::Unreachable(err.to_string())),
        }
    }
}
