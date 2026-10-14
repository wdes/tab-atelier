// SPDX-License-Identifier: MPL-2.0

//! Runtime configuration, and the upstream hop it configures.

use std::net::SocketAddr;
use std::time::Duration;

/// The daemon this crate fronts when `TAB_ATELIER_UPSTREAM` is unset.
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:7890";

/// Address the kiosk listens on when `TAB_ATELIER_KIOSK_ADDR` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:8282";

/// Runtime knobs, read from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the daemon to proxy to, no trailing slash.
    pub upstream: String,
    /// Address to listen on.
    pub bind: SocketAddr,
}

impl Config {
    /// Validate a pair of knobs, from the environment or the command line.
    ///
    /// # Errors
    ///
    /// Fails when the upstream is not an http(s) URL or the bind address is
    /// unparseable.
    pub fn new(upstream: &str, bind: &str) -> Result<Self, String> {
        let upstream = upstream.trim_end_matches('/').to_string();
        if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
            return Err(format!("upstream must be an http(s) URL, got {upstream:?}"));
        }
        let bind = bind
            .parse()
            .map_err(|err| format!("bind address {bind:?} is not an address: {err}"))?;
        Ok(Self { upstream, bind })
    }

    /// Read `TAB_ATELIER_UPSTREAM` and `TAB_ATELIER_KIOSK_ADDR`, falling back
    /// to [`DEFAULT_UPSTREAM`] and [`DEFAULT_BIND`].
    ///
    /// # Errors
    ///
    /// As [`Config::new`].
    pub fn from_env() -> Result<Self, String> {
        let upstream = std::env::var("TAB_ATELIER_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
        let bind = std::env::var("TAB_ATELIER_KIOSK_ADDR").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        Self::new(&upstream, &bind)
    }
}

/// Deadlines for the upstream hop, one per stage that must produce something.
///
/// Split out from [`Upstream`] so a test can compress them and still exercise
/// the real timeout paths instead of waiting out the production values.
#[derive(Debug, Clone, Copy)]
pub struct HopTimeouts {
    pub resolve: Option<Duration>,
    pub connect: Option<Duration>,
    pub send_request: Option<Duration>,
    pub send_body: Option<Duration>,
    /// Deadline for the RESPONSE HEADERS to arrive.
    ///
    /// `None` on purpose, and it must stay that way. ureq keeps this stage armed
    /// while the body is being read: its deadline is fixed at the moment the
    /// headers landed, so a non-`None` value caps the TOTAL life of the body,
    /// killing a healthy stream at a fixed age. Waiting for headers is still
    /// bounded — the `send_request` deadline covers that window.
    pub recv_response: Option<Duration>,
    /// Idle deadline on the response body: a stream that keeps producing
    /// survives however long it likes, one that goes quiet this long is dead.
    /// This, not `recv_response`, is what bounds a body.
    pub recv_body: Option<Duration>,
}

impl Default for HopTimeouts {
    fn default() -> Self {
        Self {
            resolve: Some(Duration::from_secs(5)),
            connect: Some(Duration::from_secs(10)),
            send_request: Some(Duration::from_secs(10)),
            send_body: Some(Duration::from_secs(30)),
            recv_response: None,
            recv_body: Some(Duration::from_mins(5)),
        }
    }
}

/// The daemon base URL plus the agent used to reach it.
///
/// One agent for the whole process: it owns the connection pool, and it is the
/// single place the hop's timeouts are configured.
#[derive(Debug)]
pub struct Upstream {
    pub base: String,
    pub agent: ureq::Agent,
}

impl Upstream {
    pub fn new(base: String, timeouts: HopTimeouts) -> Self {
        let agent = ureq::Agent::config_builder()
            // A 404 from the daemon is a 404 for the browser, not an upstream
            // failure to be rewritten into a 502.
            .http_status_as_error(false)
            // The browser follows redirects, and its cookie/authorization
            // handling must see the real target.
            .max_redirects(0)
            .timeout_resolve(timeouts.resolve)
            .timeout_connect(timeouts.connect)
            .timeout_send_request(timeouts.send_request)
            .timeout_send_body(timeouts.send_body)
            .timeout_recv_response(timeouts.recv_response)
            .timeout_recv_body(timeouts.recv_body)
            .build()
            .new_agent();
        Self { base, agent }
    }
}
