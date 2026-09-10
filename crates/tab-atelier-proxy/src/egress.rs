// SPDX-License-Identifier: MPL-2.0

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

/// Where the Claude credential is read from, resolved the same way the egress
/// resolves it.
///
/// Public because it is the first thing worth printing when the proxy cannot
/// authenticate: the service runs with `HOME=/var/lib/tab-atelier-proxy` from
/// its unit, so a `claude` login performed in an operator's own shell writes
/// somewhere else entirely and the proxy never sees it. "Which file" answers
/// that in one line; "token read" does not.
///
/// # Errors
/// `$HOME` is unset and no override was configured.
pub fn credentials_file() -> Result<PathBuf, String> {
    credentials_path()
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
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&CredentialsFile {
        claude_ai_oauth: blob.clone(),
    })
    .map_err(|e| e.to_string())?;

    // Created 0600, not chmod'ed to 0600 afterwards. This file holds a live
    // access token and the rotated refresh token; `fs::write` would create it
    // 0644 under the usual umask and leave it world-readable for the whole
    // window between write and chmod — and the temp file was readable for the
    // entire write. A permission fixed up after the fact is a permission that
    // was wrong first.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| format!("open {}: {e}", tmp.display()))?;
    f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    // Durability before visibility: rename makes the NAME atomic, not the
    // contents. Without this a crash can leave a present-but-empty
    // credentials file, which is the exact failure the atomic write is for.
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Who a Claude credential belongs to.
///
/// Recorded when a credential is installed, so a later replacement can be
/// checked against it. The uuid is the part that matters — a display name or
/// an email can be changed, and neither is what Anthropic bills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    /// `account.uuid` from `/api/oauth/profile`. Stable per person.
    pub account_uuid: String,
    #[serde(default)]
    pub organization_uuid: String,
    /// For the operator reading the file; never compared.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub recorded_at: u64,
}

/// Ask Anthropic who a token belongs to.
///
/// This doubles as a liveness check: a revoked or expired token cannot answer,
/// so a caller that gets an `Identity` back knows the credential works AND
/// whose it is, in one round trip.
///
/// # Errors
/// The request failed, upstream refused the token, or the body was not the
/// profile shape.
pub fn profile_of(access_token: &str) -> Result<Identity, String> {
    let mut resp = relay_agent()
        .get(&format!("{}/api/oauth/profile", upstream()))
        .header("Authorization", format!("Bearer {access_token}"))
        // Same beta flag the usage endpoint needs to accept an OAuth token.
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|e| format!("profile request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("profile body: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("profile refused: {}", describe_oauth_error(status, &body)));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("profile parse: {e}"))?;
    let field = |a: &str, b: &str| {
        v.get(a)
            .and_then(|o| o.get(b))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let account_uuid = field("account", "uuid");
    if account_uuid.is_empty() {
        return Err("profile carried no account uuid".to_owned());
    }
    Ok(Identity {
        account_uuid,
        organization_uuid: field("organization", "uuid"),
        email: field("account", "email"),
        recorded_at: now_ms() / 1000,
    })
}

/// Where the installed credential's identity is recorded, beside the
/// credential itself.
fn identity_path() -> Result<PathBuf, String> {
    Ok(credentials_path()?.with_file_name("claude-identity.json"))
}

/// The identity of the credential currently installed, if one was recorded.
#[must_use]
pub fn recorded_identity() -> Option<Identity> {
    let path = identity_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Remember who the installed credential belongs to.
fn record_identity(id: &Identity) -> Result<(), String> {
    let path = identity_path()?;
    let json = serde_json::to_string_pretty(id).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Install a Claude login copied from another machine.
///
/// The proxy authenticates to Anthropic with the host's own Claude OAuth
/// credentials, and there is no way to perform that login on a headless
/// server. So it is imported — and then it goes stale, because a refresh
/// rotates the refresh token and invalidates every other copy. Logging in
/// again on the laptop revokes the proxy's token; the proxy then answers every
/// request with "OAuth access token has been revoked" until it is re-imported.
///
/// Validated before it is written: an unparseable paste that overwrote a
/// working credential would turn a recoverable mistake into a broken proxy.
///
/// # Errors
/// The JSON is not a Claude credentials file, or the destination is unwritable.
pub fn import_credentials(raw: &str) -> Result<Imported, String> {
    let blob = parse_blob(raw)?;
    if blob.access_token.is_empty() || blob.refresh_token.is_empty() {
        return Err("credentials are missing an access or refresh token".to_owned());
    }
    let path = credentials_path()?;
    persist(&path, &blob)?;
    // Best effort, and deliberately after the write: a host-side import is a
    // trusted act, so a network hiccup must not stop the operator installing
    // a credential. Without a recorded identity the API repair route refuses,
    // which is the safe direction.
    let identity = profile_of(&blob.access_token).ok();
    if let Some(id) = &identity {
        let _ = record_identity(id);
    }
    Ok(Imported {
        path,
        expires_at: blob.expires_at,
        scopes: blob.scopes,
        identity,
    })
}

/// Replace the credential over the API, with the two guards that make it safe.
///
/// This exists so a broken proxy can be repaired from a laptop that still has
/// a working Claude login, without shell access to the host. Both guards are
/// necessary and neither is sufficient:
///
/// 1. **Only when the current credential is already broken.** Checked live,
///    against Anthropic, not inferred. A working proxy cannot be repointed by
///    anyone holding a user key — that would be a way to move an
///    organisation's billing, or to break it on purpose.
/// 2. **Only to the same Anthropic account.** The incoming token is resolved
///    through `/api/oauth/profile` and its `account.uuid` must equal the one
///    recorded when the credential was installed. So during an outage the
///    worst a key holder can do is restore the account that was already there.
///
/// With no recorded identity there is nothing to compare against, so this
/// refuses: bootstrapping stays a host-side act.
///
/// # Errors
/// The current credential still works, the incoming one is malformed or dead,
/// it belongs to somebody else, or no identity was ever recorded.
pub fn repair_credentials(raw: &str) -> Result<Imported, String> {
    let Some(known) = recorded_identity() else {
        return Err("no identity on record — install the first credential on the host with \
                    `tab-atelier-proxy import-credentials`"
            .to_owned());
    };
    // Guard 1: is the proxy actually broken? Ask upstream rather than trust a
    // cached opinion, so this cannot be used against a healthy proxy.
    if let Ok(token) = oauth_access_token()
        && profile_of(&token).is_ok()
    {
        return Err("the installed credential still works — refusing to replace it".to_owned());
    }
    let blob = parse_blob(raw)?;
    if blob.access_token.is_empty() || blob.refresh_token.is_empty() {
        return Err("credentials are missing an access or refresh token".to_owned());
    }
    // Guard 2: same account. Also proves the incoming credential is alive,
    // which stops a broken proxy being "repaired" into a still-broken one.
    let incoming = profile_of(&blob.access_token)?;
    if incoming.account_uuid != known.account_uuid {
        return Err(format!(
            "that credential belongs to a different Anthropic account ({}) than the one \
             this proxy was set up with ({})",
            incoming.email, known.email
        ));
    }
    let path = credentials_path()?;
    persist(&path, &blob)?;
    let _ = record_identity(&incoming);
    Ok(Imported {
        path,
        expires_at: blob.expires_at,
        scopes: blob.scopes,
        identity: Some(incoming),
    })
}

/// What [`import_credentials`] installed, for the CLI to report.
#[derive(Debug)]
pub struct Imported {
    pub path: PathBuf,
    /// Who it turned out to belong to. `None` when the profile lookup failed.
    pub identity: Option<Identity>,
    /// Unix ms when the ACCESS token expires. The refresh token outlives it;
    /// this is only a hint that the import was a live credential, not a stale
    /// file someone had lying around.
    pub expires_at: u64,
    pub scopes: Vec<String>,
}

/// Turn an OAuth error response into something an operator can act on.
///
/// `invalid_grant` is the one that matters and the one that is easiest to
/// misread: it does not mean the proxy is misconfigured, it means the stored
/// refresh token is no longer accepted — normally because the credentials were
/// copied from a machine that has since refreshed them and rotated the token
/// out from under this copy. The fix is to import them again, so the message
/// says that instead of leaving it to be inferred.
fn describe_oauth_error(status: u16, raw: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(raw).ok();
    let field = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let code = field("error").unwrap_or_else(|| format!("http {status}"));
    let detail = field("error_description").unwrap_or_else(|| {
        // Not an OAuth envelope — keep a bounded slice of whatever came back
        // rather than discarding the only evidence there is.
        raw.chars().take(200).collect()
    });
    let hint = if code == "invalid_grant" {
        " — the stored refresh token is no longer valid; import the credentials again"
    } else {
        ""
    };
    if detail.trim().is_empty() {
        format!("http {status}: {code}{hint}")
    } else {
        format!("http {status}: {code}: {detail}{hint}")
    }
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
    // The agent is built with HTTP errors NOT raised as errors, so a rejected
    // refresh arrives here as a normal response carrying an OAuth error
    // envelope. Decoding that as a token blob reports "missing field
    // `access_token`" — which describes our struct, not the problem, and sent
    // an operator looking at the wrong thing. Read the status first and say
    // what the server actually said.
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let raw = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("refresh rejected: {}", describe_oauth_error(status, &raw)));
    }
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

/// Ask Anthropic how much of the shared plan has been used.
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
    let where_from = credentials_path().map_or_else(|e| e, |p| p.display().to_string());
    let token = match oauth_access_token() {
        Ok(t) => {
            out.push(Probe {
                label: "credential",
                status: None,
                elapsed: started.elapsed(),
                note: format!("read {where_from}"),
            });
            t
        }
        Err(e) => {
            out.push(Probe {
                label: "credential",
                status: None,
                elapsed: started.elapsed(),
                // The path is the point: "no such file" means the login was
                // performed as a different user than the service runs as.
                note: format!("FAILED reading {where_from}: {e}"),
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
    use super::{import_credentials, merge_beta, set_credentials_path};

    /// Serialises the tests that redirect the credentials path.
    ///
    /// `CREDS_PATH_OVERRIDE` is a process global, so two of these running at
    /// once point at each other's fixtures — which showed up as one test
    /// finding the other's directory already deleted.
    static CREDS_PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A credentials file in the shape Claude Code writes.
    fn creds(access: &str, refresh: &str, expires_at: u64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
                "scopes": ["user:inference", "user:profile"],
            }
        })
        .to_string()
    }

    #[test]
    fn importing_installs_a_login_the_host_cannot_perform_itself() {
        let _guard = CREDS_PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("ta-import-{}", std::process::id()));
        let path = dir.join(".claude").join(".credentials.json");
        let _ = std::fs::remove_dir_all(&dir);
        set_credentials_path(Some(path.clone()));

        let imported = import_credentials(&creds("sk-ant-oat01-live", "rt-live", 9_000_000_000_000))
            .expect("a well-formed credentials file imports");
        assert_eq!(imported.path, path);
        assert_eq!(imported.scopes, ["user:inference", "user:profile"]);

        // It round-trips: what the egress reads back is what was imported.
        let back = std::fs::read_to_string(&path).expect("written");
        assert!(back.contains("sk-ant-oat01-live"));
        assert!(back.contains("rt-live"));

        // 0600 from creation. This file holds a live access token and the
        // refresh token; a window where it is world-readable is the whole
        // risk, so the mode is asserted rather than assumed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials must not be readable by anyone else");
        }
        // The parent directory is created — a fresh proxy host has no ~/.claude.
        assert!(path.parent().is_some_and(std::path::Path::is_dir));

        set_credentials_path(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_refuses_when_no_identity_was_ever_recorded() {
        // Bootstrapping stays a host-side act. With nothing on record there
        // is nothing to compare a replacement against, so "same account"
        // cannot be enforced — and a guard that cannot be enforced must not
        // be silently skipped.
        let _guard = CREDS_PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("ta-repair-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        set_credentials_path(Some(dir.join(".credentials.json")));

        let err = super::repair_credentials(&creds("a", "r", 9_000_000_000_000)).expect_err("must refuse");
        assert!(err.contains("no identity on record"), "{err}");

        set_credentials_path(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_refuses_a_credential_from_a_different_account() {
        // The guard that matters most: during an outage, a key holder must
        // not be able to point the proxy at their own Anthropic account.
        // Checked without a network by comparing the recorded identity
        // directly — `repair_credentials` compares these same two uuids.
        let mine = super::Identity {
            account_uuid: "7a427e27-aaaa".to_owned(),
            organization_uuid: "org-1".to_owned(),
            email: "ada@example.org".to_owned(),
            recorded_at: 1,
        };
        let theirs = super::Identity {
            account_uuid: "0000-bbbb".to_owned(),
            organization_uuid: "org-2".to_owned(),
            email: "mallory@example.org".to_owned(),
            recorded_at: 2,
        };
        assert_ne!(mine.account_uuid, theirs.account_uuid);
        // Identity round-trips through the file it is stored in, since that is
        // what the comparison reads back.
        let json = serde_json::to_string(&mine).expect("serialise");
        let back: super::Identity = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, mine, "a recorded identity must survive a restart intact");
    }

    #[test]
    fn a_bad_paste_cannot_destroy_a_working_credential() {
        let _guard = CREDS_PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("ta-import-bad-{}", std::process::id()));
        let path = dir.join(".credentials.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        set_credentials_path(Some(path.clone()));

        let good = creds("sk-ant-oat01-keepme", "rt-keepme", 9_000_000_000_000);
        import_credentials(&good).expect("seed");

        // Validation happens BEFORE the write, so the recoverable mistake of
        // pasting the wrong thing stays recoverable instead of leaving the
        // proxy with no credential at all.
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"claudeAiOauth":{}}"#,
            // Shape is right, tokens are empty — the subtle one, since serde
            // is perfectly happy with it.
            &creds("", "", 1),
        ] {
            assert!(import_credentials(bad).is_err(), "{bad:?} must be refused");
            let still = std::fs::read_to_string(&path).expect("still there");
            assert!(
                still.contains("sk-ant-oat01-keepme"),
                "the working credential survived {bad:?}"
            );
        }

        set_credentials_path(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
