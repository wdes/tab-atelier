// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Anthropic API relay — egress credential + forwarding helpers.
//!
//! The relay lets a LOCAL tab-atelier forward its Claude tabs' Anthropic API
//! calls through a REMOTE tab-atelier (see [`crate::RELAY_MODE`]). This module
//! is the **egress** side, which runs on the remote and reuses the remote's own
//! Claude login (Option A — no API key): it reads `~/.claude/.credentials.json`,
//! refreshes the OAuth token when near expiry, and injects the same headers
//! Claude Code sends.
//!
//! The credential schema + refresh flow are ported from
//! `crates/catbus-agent/src/auth.rs` (the source of truth) into this crate's
//! sync/`ureq` world, so the egress doesn't pull `reqwest`/`tokio` in.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// OAuth client id Claude Code registers as (from `catbus-agent`'s `auth.rs`).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// `anthropic-beta` header Claude Code sends for OAuth-authenticated requests.
pub const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Upstream the egress forwards to.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
/// Refresh this many ms before the access token expires so an in-flight
/// request can't race the rollover.
const REFRESH_LEAD_MS: u64 = 60_000;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OauthBlob {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default)]
    scopes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialsFile {
    claude_ai_oauth: OauthBlob,
}

/// Test/ops override for the credentials file location. Set via
/// [`set_credentials_path`]; falls back to `~/.claude/.credentials.json`.
static CREDS_PATH_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Point the egress at a different credentials file (tests use a fixture; ops
/// could point at a service account's creds). `None` restores the default.
pub fn set_credentials_path(path: Option<PathBuf>) {
    if let Ok(mut g) = CREDS_PATH_OVERRIDE.write() {
        *g = path;
    }
}

fn credentials_path() -> Result<PathBuf, String> {
    if let Some(p) = CREDS_PATH_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Ok(p);
    }
    let home = std::env::var_os("HOME").ok_or("no $HOME — can't locate ~/.claude/.credentials.json")?;
    Ok(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

/// Test/ops override for the egress upstream. Set via [`set_upstream`], else the
/// `TAB_ATELIER_RELAY_UPSTREAM` env var, else [`ANTHROPIC_BASE`].
static UPSTREAM_OVERRIDE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Point the egress at a different upstream (tests use a mock Anthropic).
pub fn set_upstream(url: Option<String>) {
    if let Ok(mut g) = UPSTREAM_OVERRIDE.write() {
        *g = url;
    }
}

/// The egress upstream base URL (no trailing slash).
#[must_use]
pub fn upstream() -> String {
    if let Some(u) = UPSTREAM_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return u.trim_end_matches('/').to_owned();
    }
    std::env::var("TAB_ATELIER_RELAY_UPSTREAM")
        .map_or_else(|_| ANTHROPIC_BASE.to_owned(), |u| u.trim_end_matches('/').to_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A ureq agent for relay/egress calls.
///
/// **No global timeout** (LLM streams run for minutes) and **default `WebPKI`
/// verification** (unlike the LAN self-signed remote agent). A connect timeout
/// still bounds a dead upstream.
#[must_use]
pub fn relay_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!("tab-atelier/", env!("CARGO_PKG_VERSION"), " (relay)"))
        .build()
        .new_agent()
}

/// Return a currently-valid Claude OAuth access token.
///
/// Refreshes (and persists the rotated blob back, 0600) when it's within
/// [`REFRESH_LEAD_MS`] of expiry. Reads the credentials file each call — cheap,
/// and keeps the egress stateless.
///
/// # Errors
/// Returns a message when `$HOME`/the credentials file is missing or malformed,
/// or the refresh request fails — the route turns it into a 502.
pub fn oauth_access_token() -> Result<String, String> {
    let path = credentials_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let blob = parse_blob(&raw)?;
    if blob.expires_at > now_ms() + REFRESH_LEAD_MS {
        return Ok(blob.access_token);
    }
    // Refresh rotates the refresh token → persist atomically (a partial write
    // would brick auth).
    let fresh = refresh(&blob.refresh_token)?;
    persist(&path, &fresh)?;
    Ok(fresh.access_token)
}

fn parse_blob(raw: &str) -> Result<OauthBlob, String> {
    serde_json::from_str::<CredentialsFile>(raw)
        .map(|c| c.claude_ai_oauth)
        .map_err(|e| format!("malformed credentials: {e}"))
}

fn persist(path: &std::path::Path, blob: &OauthBlob) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&CredentialsFile {
        claude_ai_oauth: blob.clone(),
    })
    .map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn refresh(refresh_token: &str) -> Result<OauthBlob, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        grant_type: &'static str,
        refresh_token: &'a str,
        client_id: &'static str,
    }
    #[derive(Deserialize)]
    struct Resp {
        access_token: String,
        refresh_token: String,
        // Anthropic returns `expires_in` seconds; we compute an absolute
        // deadline ourselves.
        expires_in: u64,
        #[serde(default)]
        scope: Option<String>,
    }
    let mut resp = relay_agent()
        .post(REFRESH_URL)
        .send_json(Req {
            grant_type: "refresh_token",
            refresh_token,
            client_id: CLIENT_ID,
        })
        .map_err(|e| format!("refresh request failed: {e}"))?;
    let body: Resp = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("refresh decode: {e}"))?;
    Ok(OauthBlob {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        expires_at: now_ms() + body.expires_in * 1000,
        scopes: body
            .scope
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_claude_credentials_schema() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","refreshToken":"sk-ant-ort01-def","expiresAt":1748276587173,"scopes":["user:inference"]}}"#;
        let blob = parse_blob(raw).expect("parse");
        assert_eq!(blob.access_token, "sk-ant-oat01-abc");
        assert_eq!(blob.refresh_token, "sk-ant-ort01-def");
        assert_eq!(blob.expires_at, 1_748_276_587_173);
    }

    #[test]
    fn missing_oauth_key_is_an_error() {
        assert!(parse_blob(r#"{"nope":true}"#).is_err());
    }

    #[test]
    fn egress_header_constants_match_claude_code() {
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
        assert!(ANTHROPIC_BETA.contains("oauth-2025-04-20"));
        assert!(ANTHROPIC_BETA.contains("claude-code-20250219"));
    }
}
