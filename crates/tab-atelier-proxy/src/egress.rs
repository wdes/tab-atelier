// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Anthropic egress — the proxy's own Claude credential.
//!
//! A tab-atelier forwards its Claude tabs' Anthropic calls here; this module is
//! what happens at the far end. It reuses the proxy host's own Claude login
//! rather than an API key: it reads `~/.claude/.credentials.json`, refreshes
//! the OAuth token when it is near expiry, and injects the headers Claude Code
//! sends.
//!
//! ONE credential, MANY accounts. The users each authenticate with their own
//! key (see [`crate::users`]); this Claude login is the proxy's, shared by all
//! of them, and never leaves the machine. That split is the point: a key that
//! leaks lets someone spend the proxy's quota until it is revoked, and cannot
//! be replayed against Anthropic directly.
//!
//! The credential schema + refresh flow come from `catbus-agent`'s `auth.rs`
//! (the source of truth), rendered in `ureq` so the proxy does not pull
//! `reqwest`/`tokio` into its egress path.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// OAuth client id Claude Code registers as (from `catbus-agent`'s `auth.rs`).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// `anthropic-beta` header Claude Code sends for OAuth-authenticated requests.
pub const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Merge the client's `anthropic-beta` with the flags OAuth requires.
///
/// The egress authenticates with a Claude Code OAuth token, which upstream
/// only accepts alongside [`ANTHROPIC_BETA`] — so those flags must be present.
/// We used to just *set* the header to that constant, which silently dropped
/// whatever the client had opted into. A client that sends a body field gated
/// behind its own beta flag (`context_management`, say) then gets
/// `400 … extra inputs are not permitted` from upstream: the field arrived,
/// the opt-in did not. So take the union instead, keeping the client's order
/// and appending ours, case-insensitively deduplicated.
#[must_use]
pub fn merge_beta(client: Option<&str>, required: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for flag in client
        .unwrap_or("")
        .split(',')
        .chain(required.split(','))
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        if !out.iter().any(|k| k.eq_ignore_ascii_case(flag)) {
            out.push(flag);
        }
    }
    out.join(",")
}

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
/// `TAB_ATELIER_PROXY_UPSTREAM` env var, else [`ANTHROPIC_BASE`].
static UPSTREAM_OVERRIDE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Point the egress at a different upstream (tests use a mock Anthropic).
pub fn set_upstream(url: Option<String>) {
    if let Ok(mut g) = UPSTREAM_OVERRIDE.write() {
        *g = url;
    }
}

/// An explicitly configured upstream, if there is one.
///
/// Distinct from [`upstream`], which always answers. Routing needs to know
/// whether an override was SET, because it replaces the base URL of the
/// subscription provider — that is what the override is for, and silently
/// ignoring it would leave a test or an ops redirect pointing nowhere.
#[must_use]
pub fn upstream_override() -> Option<String> {
    if let Some(u) = UPSTREAM_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return Some(u.trim_end_matches('/').to_owned());
    }
    std::env::var("TAB_ATELIER_PROXY_UPSTREAM")
        .ok()
        .map(|u| u.trim_end_matches('/').to_owned())
}

/// The egress upstream base URL (no trailing slash).
#[must_use]
pub fn upstream() -> String {
    if let Some(u) = UPSTREAM_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return u.trim_end_matches('/').to_owned();
    }
    std::env::var("TAB_ATELIER_PROXY_UPSTREAM")
        .map_or_else(|_| ANTHROPIC_BASE.to_owned(), |u| u.trim_end_matches('/').to_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A ureq agent for egress calls.
///
/// **No global timeout** (LLM streams run for minutes) and **default `WebPKI`
/// verification** (unlike the LAN self-signed remote agent). A connect timeout
/// still bounds a dead upstream.
///
/// `http_status_as_error(false)`: a relay must be transparent to the upstream's
/// status. ureq's default turns any non-2xx into `Err`, which would collapse a
/// real upstream 429/500/529 (with its explanatory body) into an opaque
/// synthetic 502 — the caller (Claude Code) then can't see the real reason or
/// honour Retry-After. With this off, non-2xx comes back as `Ok(resp)` and we
/// stream the true status + body through.
#[must_use]
pub fn relay_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .user_agent(concat!("tab-atelier-proxy/", env!("CARGO_PKG_VERSION")))
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

/// Ask Anthropic how much of the shared plan is left.
///
/// `GET /api/oauth/usage` with the Claude Code OAuth token — the same endpoint
/// `.claude/scripts/claude-usage-monitor.mjs` polls. It is the only source
/// that knows the subscription's real utilisation; everything else the proxy
/// can see is per-request accounting.
///
/// # Errors
/// No usable credential, or the request failed. The caller records the failure
/// as a sample rather than dropping it.
pub fn account_usage() -> Result<(u16, String), String> {
    let token = oauth_access_token()?;
    let mut resp = relay_agent()
        .get(&format!("{}/api/oauth/usage", upstream()))
        .header("Authorization", format!("Bearer {token}"))
        // Without this beta flag the endpoint refuses an OAuth credential.
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|e| format!("usage request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("usage body: {e}"))?;
    Ok((status, body))
}

/// One timed probe of the upstream API.
#[derive(Debug, Clone)]
pub struct Probe {
    pub label: &'static str,
    pub status: Option<u16>,
    pub elapsed: Duration,
    pub note: String,
}

/// Time the round trip to Anthropic, in the stages that can fail separately.
///
/// A slow proxy is usually not the proxy. Splitting the trip apart says which
/// part is slow instead of leaving an operator to guess:
///
/// * **token** — reading (and possibly refreshing) the local OAuth credential.
///   Slow here means the refresh endpoint, not the API.
/// * **connect** — DNS plus TCP plus TLS to the upstream host, measured by a
///   request that does no model work.
/// * **round trip** — a real `/v1/messages` call with a one-token completion.
///   This is the number that matters, and it is the only one that includes
///   the model actually thinking.
///
/// The last one costs a handful of tokens against the shared plan. That is the
/// price of measuring the thing people actually wait for; a HEAD request to an
/// unrelated path would measure the CDN and tell you nothing.
#[must_use]
pub fn probe_round_trip(model: &str) -> Vec<Probe> {
    let mut out = Vec::new();

    let started = std::time::Instant::now();
    let token = match oauth_access_token() {
        Ok(t) => {
            out.push(Probe {
                label: "credential",
                status: None,
                elapsed: started.elapsed(),
                note: "local OAuth token read (refreshed if it was near expiry)".to_owned(),
            });
            t
        }
        Err(e) => {
            out.push(Probe {
                label: "credential",
                status: None,
                elapsed: started.elapsed(),
                note: format!("FAILED: {e}"),
            });
            return out;
        }
    };

    let agent = relay_agent();
    let base = upstream();

    // Reachability only: the endpoint Claude Code itself probes, which does no
    // model work, so this isolates DNS + TCP + TLS from generation time.
    let started = std::time::Instant::now();
    match agent.get(&format!("{base}/api/hello")).call() {
        Ok(r) => out.push(Probe {
            label: "connect",
            status: Some(r.status().as_u16()),
            elapsed: started.elapsed(),
            note: "DNS + TCP + TLS to the API host".to_owned(),
        }),
        Err(e) => out.push(Probe {
            label: "connect",
            status: None,
            elapsed: started.elapsed(),
            note: format!("FAILED: {e}"),
        }),
    }

    // The real thing: a one-token completion.
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let started = std::time::Instant::now();
    let sent = agent
        .post(&format!("{base}/v1/messages"))
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", ANTHROPIC_BETA)
        .header("Content-Type", "application/json")
        .send_json(&body);
    match sent {
        Ok(mut r) => {
            let status = r.status().as_u16();
            let text = r.body_mut().read_to_string().unwrap_or_default();
            let note = if (200..300).contains(&status) {
                format!("{model} answered")
            } else {
                // The body is where upstream says WHY — an overloaded 529 and
                // a 400 about the model name need different responses.
                text.chars().take(160).collect::<String>()
            };
            out.push(Probe {
                label: "round trip",
                status: Some(status),
                elapsed: started.elapsed(),
                note,
            });
        }
        Err(e) => out.push(Probe {
            label: "round trip",
            status: None,
            elapsed: started.elapsed(),
            note: format!("FAILED: {e}"),
        }),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::merge_beta;

    #[test]
    fn the_clients_beta_flags_survive_the_egress() {
        // The OAuth flags are mandatory upstream, so they are always present…
        assert_eq!(merge_beta(None, ANTHROPIC_BETA), ANTHROPIC_BETA);
        assert_eq!(merge_beta(Some(""), ANTHROPIC_BETA), ANTHROPIC_BETA);
        // …but they must not evict what the client opted into, or a body field
        // gated behind the client's flag is rejected as an unknown input.
        let merged = merge_beta(Some("context-management-2025-06-27"), ANTHROPIC_BETA);
        assert!(merged.starts_with("context-management-2025-06-27,"), "{merged}");
        for required in ANTHROPIC_BETA.split(',') {
            assert!(merged.contains(required), "{required} missing from {merged}");
        }
        // Overlap is not duplicated, and spacing/case from the client is
        // tolerated — this header is assembled by several SDKs.
        let merged = merge_beta(Some(" OAUTH-2025-04-20 , fine-grained-tool-streaming "), ANTHROPIC_BETA);
        assert_eq!(
            merged.matches("oauth-2025-04-20").count() + merged.matches("OAUTH-2025-04-20").count(),
            1
        );
        assert!(merged.contains("fine-grained-tool-streaming"), "{merged}");
        assert!(!merged.contains(" ,"), "no stray spacing: {merged}");
    }

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
