// SPDX-License-Identifier: MPL-2.0

//! Where the relay lives, and how to prove we may use it.
//!
//! This module holds the *client* half of the login: the relay's address, the
//! relay token issued for this machine, and — when the relay sits behind
//! Cloudflare Access — the service token that gets us past the edge. Nothing
//! else.
//!
//! The subscription credential is not here, and must not be. The OAuth blob
//! under `~/.claude`, its refresh cycle, and the account it belongs to live on
//! the relay (`tab-atelier-proxy`) and never travel to a client. That split is
//! the point of the relay: an agent tab on a laptop, a headless box, or a phone
//! proves only that it is allowed to ask, and the proxy holds the login and
//! does the talking to Anthropic.
//!
//! Resolution order, first hit wins:
//!
//! 1. `--relay-url` / `--relay-token` (and their `CATBUS_RELAY_URL` /
//!    `CATBUS_RELAY_TOKEN` equivalents)
//! 2. the `relay_endpoint_id` endpoint in `preferences.json`, so a laptop that
//!    already runs tab-atelier needs no extra flags
//! 3. any other endpoint carrying a relay token, for the case where the id was
//!    never pinned

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The relay's Anthropic passthrough prefix. A client's base URL is the relay
/// origin plus this, and the usual `/v1/messages` is appended to that.
pub const RELAY_PATH: &str = "/relay/anthropic";

/// Path appended to the base URL to reach the Messages API.
pub const MESSAGES_PATH: &str = "/v1/messages";

/// Where the relay serves what it charges, one entry per model it can serve. Imitates the
/// provider's own models endpoint so a relay needs no new convention.
pub const MODELS_PATH: &str = "/v1/models";

const ENV_URL: &str = "CATBUS_RELAY_URL";
const ENV_TOKEN: &str = "CATBUS_RELAY_TOKEN";
/// Point the resolver at a different `preferences.json`. Tests use it; so does
/// anyone running two tab-atelier profiles.
const ENV_PREFERENCES: &str = "CATBUS_PREFERENCES";

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error(
        "no relay configured — pass --relay-url and --relay-token, set {ENV_URL} and \
         {ENV_TOKEN}, or add a relay endpoint with a relay token to {0}"
    )]
    NoConfig(String),
    #[error(
        "relay endpoint {0:?} has no relay token — mint one on the proxy and set \
         `relay_token` for that endpoint in {1}"
    )]
    NoToken(String, String),
    #[error("relay endpoint {0:?} is not in {1}")]
    UnknownEndpoint(String, String),
    #[error("parsing {0}: {1}")]
    Malformed(String, String),
}

/// A resolved relay: an absolute base URL and the credential for it.
#[derive(Debug, Clone)]
pub struct Relay {
    base_url: String,
    token: String,
    cf_access_client_id: String,
    cf_access_client_secret: String,
}

impl Relay {
    /// Build a relay from an origin (or a full base URL) and a token.
    ///
    /// Accepts either `https://proxy.example` or
    /// `https://proxy.example/relay/anthropic`, so the value can be pasted
    /// straight from `preferences.json`.
    #[must_use]
    pub fn new(url: &str, token: &str) -> Self {
        let trimmed = url.trim().trim_end_matches('/');
        let base_url = if trimmed.ends_with(RELAY_PATH) {
            trimmed.to_owned()
        } else {
            format!("{trimmed}{RELAY_PATH}")
        };
        Self {
            base_url,
            token: token.trim().to_owned(),
            cf_access_client_id: String::new(),
            cf_access_client_secret: String::new(),
        }
    }

    /// Attach a Cloudflare Access service token, for a relay published through
    /// Access. Builder-style so the common case stays a two-argument call.
    #[must_use]
    pub fn with_cloudflare_access(mut self, client_id: &str, client_secret: &str) -> Self {
        self.cf_access_client_id = client_id.trim().into();
        self.cf_access_client_secret = client_secret.trim().into();
        self
    }

    /// Absolute URL of the Messages endpoint on the relay.
    #[must_use]
    pub fn messages_url(&self) -> String {
        format!("{}{MESSAGES_PATH}", self.base_url)
    }

    /// Where the relay serves its prices.
    ///
    /// On the same origin as the messages endpoint, which is the point: a tab with no internet
    /// still reaches it, because that origin is the app's own relay. The path imitates the
    /// provider's models endpoint so a relay can serve it without inventing a convention.
    #[must_use]
    pub fn models_url(&self) -> String {
        format!("{}{MODELS_PATH}", self.base_url)
    }

    /// The relay base URL, for diagnostics. Safe to print.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The relay token. This is a bearer credential: never log it.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The Cloudflare Access service token, if one was configured.
    #[must_use]
    pub fn cloudflare_access(&self) -> Option<(&str, &str)> {
        (!self.cf_access_client_id.is_empty() && !self.cf_access_client_secret.is_empty())
            .then_some((self.cf_access_client_id.as_str(), self.cf_access_client_secret.as_str()))
    }

    /// Resolve the relay for this machine: explicit flags/env first, then the
    /// tab-atelier preferences the laptop already has.
    ///
    /// `url_override` and `token_override` are the `--relay-url` /
    /// `--relay-token` values.
    pub fn resolve(url_override: Option<&str>, token_override: Option<&str>) -> Result<Self, RelayError> {
        let url = url_override.map(str::to_owned).or_else(|| env_non_empty(ENV_URL));
        let token = token_override.map(str::to_owned).or_else(|| env_non_empty(ENV_TOKEN));

        match (url, token) {
            // Fully explicit: preferences are never read, so a broken or
            // missing preferences.json cannot stop an explicit invocation.
            (Some(url), Some(token)) => Ok(Self::new(&url, &token)),
            // Whichever half is missing comes from the endpoint, including its
            // Cloudflare Access pair. Both halves use the same lookup, so a
            // pinned endpoint that lacks a token fails the same way either way.
            (given_url, token_override) => {
                let Chosen {
                    url,
                    token,
                    cf_access_client_id,
                    cf_access_client_secret,
                } = credentials(&prefs()?, token_override.as_deref())?;
                Ok(Self::new(given_url.as_deref().unwrap_or(&url), &token)
                    .with_cloudflare_access(&cf_access_client_id, &cf_access_client_secret))
            }
        }
    }
}

/// `std::env::var` that treats an empty string as unset, so an exported-but-blank
/// variable falls through to the next source instead of becoming an empty token.
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// `preferences.json` for this user, honouring `CATBUS_PREFERENCES` and
/// `XDG_CONFIG_HOME` the way the app does.
#[must_use]
pub fn preferences_path() -> PathBuf {
    if let Some(explicit) = env_non_empty(ENV_PREFERENCES) {
        return PathBuf::from(explicit);
    }
    let config_home = env_non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env_non_empty("HOME").map(|home| Path::new(&home).join(".config")));
    config_home.map_or_else(
        || PathBuf::from("tab-atelier/preferences.json"),
        |dir| dir.join("tab-atelier").join("preferences.json"),
    )
}

/// The subset of the app's preferences this module needs.
#[derive(Debug, Deserialize)]
struct Preferences {
    #[serde(default)]
    relay_endpoint_id: Option<String>,
    #[serde(default)]
    remote_endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    relay_token: String,
    #[serde(default)]
    cf_access_client_id: String,
    #[serde(default)]
    cf_access_client_secret: String,
}

/// A relay endpoint with its token already trimmed and non-empty.
struct Chosen {
    url: String,
    token: String,
    cf_access_client_id: String,
    cf_access_client_secret: String,
}

fn prefs() -> Result<Preferences, RelayError> {
    let path = preferences_path();
    let shown = path.display().to_string();
    let raw = std::fs::read_to_string(&path).map_err(|e| RelayError::NoConfig(format!("{shown} ({e})")))?;
    serde_json::from_str(&raw).map_err(|e| RelayError::Malformed(shown, e.to_string()))
}

/// The credential for the chosen endpoint: the endpoint's stored relay token,
/// or `token_override` when the caller supplied one. The address comes back
/// too, since the caller may or may not have needed it.
fn credentials(prefs: &Preferences, token_override: Option<&str>) -> Result<Chosen, RelayError> {
    let Some(endpoint) = pinned_endpoint(prefs) else {
        // A pinned id that names nothing is a different failure from having no
        // relay at all, and saying so saves a confusing hunt through the file.
        let path = preferences_path().display().to_string();
        return Err(match prefs.relay_endpoint_id.as_deref() {
            Some(id) => RelayError::UnknownEndpoint(id.to_owned(), path),
            None => RelayError::NoConfig(path),
        });
    };

    let token = token_override
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| endpoint.relay_token.trim().to_owned());
    if token.is_empty() {
        // Reached when the endpoint simply has no relay token — a direct
        // remote tab-atelier endpoint is stored exactly like this. Checking
        // beats sending an empty `x-api-key` and reading a 401 as "wrong key".
        return Err(RelayError::NoToken(
            endpoint.id.clone(),
            preferences_path().display().to_string(),
        ));
    }

    Ok(Chosen {
        url: endpoint.url.trim().to_owned(),
        token,
        cf_access_client_id: endpoint.cf_access_client_id.trim().to_owned(),
        cf_access_client_secret: endpoint.cf_access_client_secret.trim().to_owned(),
    })
}

/// The pinned endpoint when it still exists, else the first one with a relay
/// token. Shared by every lookup so they can never disagree about which
/// endpoint "the relay" means.
fn pinned_endpoint(prefs: &Preferences) -> Option<&Endpoint> {
    prefs
        .relay_endpoint_id
        .as_deref()
        .and_then(|id| prefs.remote_endpoints.iter().find(|e| e.id == id))
        .or_else(|| prefs.remote_endpoints.iter().find(|e| !e.relay_token.trim().is_empty()))
}
