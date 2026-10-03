// SPDX-License-Identifier: MPL-2.0

//! The Anthropic API client — every URL, header and credential in one place.
//!
//! Three crates in this workspace talk to Anthropic: the desktop relay
//! ([`tab-atelier`]), the multi-user proxy (`tab-atelier-proxy`) and the
//! standalone agent (`catbus-agent`). Each grew its own copy of the endpoint
//! constants, the beta-flag list, the credential schema and the refresh flow —
//! and copies drift. The proxy's copy had gained an OAuth error decoder and a
//! 0600-from-creation write that the desktop's had not; the desktop's half was
//! dead code nobody had noticed. This module is the single copy.
//!
//! # Matching Claude Code on the wire
//!
//! Anthropic's OAuth path is for Claude Code, so requests carrying a Claude
//! Code credential have to look like Claude Code — not like us. The header set
//! below is transcribed from the shipped binary (see
//! `notes/claude-code-http-internals.md` in the `claude-code` fork, read out of
//! the bundled JS at version [`CLAUDE_CODE_VERSION`]) and confirmed against a
//! real request captured off the wire.
//!
//! Two header sets, because Claude Code itself uses two:
//!
//! * [`api_headers`] — the inference/API client (`$U()`/`Tx()`): carries the
//!   `claude-cli/…` User-Agent, `x-app` and the session id. Used for
//!   `/v1/messages`, `/api/oauth/usage` and `/api/hello`.
//! * [`oauth_headers`] — the token/profile/roles calls, which in the real
//!   client are plain `axios` requests that set none of the above.
//!
//! Getting this wrong is not cosmetic: the OAuth flags in [`ANTHROPIC_BETA`]
//! are *required* for an OAuth token to be accepted at all.
//!
//! [`tab-atelier`]: https://github.com/wdes/tab-atelier

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Anthropic API root. Everything the inference and OAuth-metadata paths use.
pub const BASE_API_URL: &str = "https://api.anthropic.com";

/// OAuth token endpoint — authorization-code exchange AND refresh.
///
/// Claude Code spells this `platform.claude.com`. The older
/// `console.anthropic.com/v1/oauth/token` is the same backend (both answer an
/// invalid grant identically, verified), so this is a rename rather than a
/// migration — but the client should say what the client says, because the
/// alias is the thing that will eventually be retired.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// OAuth client id Claude Code registers as. Public by design in a PKCE
/// public-client flow: there is no secret, the code verifier is the proof.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Where a browser is sent to authorise, logging in with a Claude.ai account.
pub const CLAUDE_AI_AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
/// Where a browser is sent to authorise, logging in with a Console account.
pub const CONSOLE_AUTHORIZE_URL: &str = "https://platform.claude.com/oauth/authorize";
/// Redirect target for the paste-the-code flow, when no loopback listener can
/// be reached (a headless box, or a browser on another machine).
pub const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";

/// Who a token belongs to, and whether it is still alive.
pub const PROFILE_PATH: &str = "/api/oauth/profile";
/// Plan utilisation. The only source that knows the subscription's real spend.
pub const USAGE_PATH: &str = "/api/oauth/usage";
/// Reachability probe that does no model work — Claude Code's own.
pub const HELLO_PATH: &str = "/api/hello";
/// The Messages API.
pub const MESSAGES_PATH: &str = "/v1/messages";

// ---------------------------------------------------------------------------
// Versions and beta flags
// ---------------------------------------------------------------------------

/// The Claude Code build this client mimics.
///
/// Pinned rather than detected: the proxy runs on a host where `claude` may
/// not be installed at all (the credential can be imported), so there is
/// nothing reliable to read a version out of. It appears in [`user_agent`], so
/// bumping this one constant is the whole update.
pub const CLAUDE_CODE_VERSION: &str = "2.1.266";

/// `anthropic-version` — the dated API contract, unrelated to the client build.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The beta flag that makes Anthropic accept an OAuth token at all.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The beta flag identifying the caller as Claude Code.
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";

/// Beta flags every OAuth-authenticated inference request must carry.
///
/// Both are load-bearing: without [`OAUTH_BETA`] the token is refused, and
/// without [`CLAUDE_CODE_BETA`] the Claude Code system-prompt path is not
/// available. Merge rather than replace a client's own flags — see
/// [`merge_beta`].
pub const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Scopes requested on a fresh login.
pub const LOGIN_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Scopes sent on a refresh — the login set minus `org:create_api_key`, which
/// is a one-time consent rather than a standing capability.
pub const REFRESH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// The two scopes that gate [`USAGE_PATH`]. Claude Code skips the call
/// entirely without both, and so should anything reading a credential that
/// might have been minted for something narrower.
pub const USAGE_REQUIRED_SCOPES: &[&str] = &["user:inference", "user:profile"];

// ---------------------------------------------------------------------------
// User-Agent and headers
// ---------------------------------------------------------------------------

/// The User-Agent Claude Code's API client sends.
///
/// Grammar, from `Tx()` in the bundle:
///
/// ```text
/// claude-cli/<VERSION> (external, <entrypoint>[, agent-sdk/<v>][, client-app/<app>])
/// ```
///
/// `external` is hardcoded upstream — it distinguishes public builds from
/// Anthropic-internal ones. The entrypoint is `CLAUDE_CODE_ENTRYPOINT`,
/// defaulting to `cli`; a captured `claude -p` run showed `sdk-cli`, which is
/// that variable and not a different grammar.
///
/// The optional segments are honoured because a caller running us underneath
/// an SDK should look like that, rather than silently claiming to be a bare
/// terminal session.
#[must_use]
pub fn user_agent() -> String {
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").unwrap_or_else(|_| "cli".to_owned());
    let sdk = std::env::var("CLAUDE_AGENT_SDK_VERSION").map_or_else(|_| String::new(), |v| format!(", agent-sdk/{v}"));
    let app =
        std::env::var("CLAUDE_AGENT_SDK_CLIENT_APP").map_or_else(|_| String::new(), |v| format!(", client-app/{v}"));
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, {entrypoint}{sdk}{app})")
}

/// Headers Claude Code's API client puts on every inference-path request.
///
/// From `$U()`: `x-app`, the User-Agent and the session id. `session_id` is
/// the caller's own — a proxy should pass the CLIENT's through rather than
/// invent one, so that a support question about one session can be traced.
///
/// This deliberately does NOT include `Authorization`/`x-api-key` (the caller
/// knows which credential it holds) or `anthropic-beta` (which must be merged
/// with the client's — see [`merge_beta`]).
#[must_use]
pub fn api_headers(session_id: Option<&str>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("User-Agent", user_agent()),
        ("x-app", "cli".to_owned()),
        ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
    ];
    if let Some(id) = session_id {
        h.push(("X-Claude-Code-Session-Id", id.to_owned()));
    }
    h
}

/// Headers for the OAuth token, profile and roles calls.
///
/// These are `axios` requests in the real client and carry none of the
/// inference client's identity headers — no `x-app`, no session id. The
/// User-Agent is still sent: something goes on the wire regardless, and a
/// `claude-cli/…` string is a closer match to what Anthropic sees from a real
/// Claude Code host than this crate's own name would be.
#[must_use]
pub fn oauth_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", user_agent()),
        ("Content-Type", "application/json".to_owned()),
    ]
}

/// Merge the client's `anthropic-beta` with the flags OAuth requires.
///
/// The egress authenticates with a Claude Code OAuth token, which upstream
/// only accepts alongside [`ANTHROPIC_BETA`] — so those flags must be present.
/// Setting the header to that constant outright silently dropped whatever the
/// client had opted into: a client that sends a body field gated behind its
/// own flag (`context_management`, say) then gets `400 … extra inputs are not
/// permitted`, because the field arrived and the opt-in did not. So take the
/// union, keeping the client's order and appending ours, case-insensitively
/// deduplicated.
///
/// A real client sends around ten flags, so this is not a hypothetical.
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

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Refresh this many ms before the access token expires so an in-flight
/// request can't race the rollover.
pub const REFRESH_LEAD_MS: u64 = 60_000;

/// Claude Code's stored OAuth blob, spelled as it is on disk.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OauthBlob {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix **milliseconds** when the access token expires.
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl OauthBlob {
    /// Whether this token needs refreshing before use.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        self.expires_at <= now_ms() + REFRESH_LEAD_MS
    }

    /// Whether the scopes allow [`USAGE_PATH`].
    #[must_use]
    pub fn can_read_usage(&self) -> bool {
        USAGE_REQUIRED_SCOPES
            .iter()
            .all(|need| self.scopes.iter().any(|s| s == need))
    }
}

/// `~/.claude/.credentials.json` — the one-key wrapper Claude Code writes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsFile {
    pub claude_ai_oauth: OauthBlob,
}

/// Parse a credentials file.
///
/// # Errors
/// The JSON is not a Claude credentials file.
pub fn parse_credentials(raw: &str) -> Result<OauthBlob, String> {
    serde_json::from_str::<CredentialsFile>(raw)
        .map(|c| c.claude_ai_oauth)
        .map_err(|e| format!("malformed credentials: {e}"))
}

/// `$HOME/.claude/.credentials.json`.
///
/// # Errors
/// `$HOME` is unset.
pub fn default_credentials_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no $HOME — can't locate ~/.claude/.credentials.json")?;
    Ok(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

/// Write a credentials file atomically, 0600, durably.
///
/// Created 0600, not chmod'ed to 0600 afterwards. This file holds a live
/// access token and the rotated refresh token; `fs::write` would create it
/// 0644 under the usual umask and leave it world-readable for the whole window
/// between write and chmod — and the temp file was readable for the entire
/// write. A permission fixed up after the fact is a permission that was wrong
/// first.
///
/// `sync_all` before the rename is durability before visibility: rename makes
/// the NAME atomic, not the contents. Without it a crash can leave a
/// present-but-empty credentials file, which is the exact failure the atomic
/// write exists to prevent.
///
/// # Errors
/// The destination directory cannot be created, or the write fails.
pub fn persist_credentials(path: &Path, blob: &OauthBlob) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&CredentialsFile {
        claude_ai_oauth: blob.clone(),
    })
    .map_err(|e| e.to_string())?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| format!("open {}: {e}", tmp.display()))?;
    f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Who a Claude credential belongs to.
///
/// Recorded when a credential is installed, so a later replacement can be
/// checked against it. The uuid is the part that matters — a display name or
/// an email can be changed, and neither is what Anthropic bills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    /// `account.uuid` from [`PROFILE_PATH`]. Stable per person.
    pub account_uuid: String,
    #[serde(default)]
    pub organization_uuid: String,
    /// For the operator reading the file; never compared.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub recorded_at: u64,
}

/// Pull an [`Identity`] out of a profile response body.
///
/// # Errors
/// The body is not JSON, or carries no account uuid — which is the only field
/// the same-account guard can be built on.
pub fn parse_profile(body: &str) -> Result<Identity, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("profile parse: {e}"))?;
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

/// Turn an OAuth error response into something an operator can act on.
///
/// `invalid_grant` is the one that matters and the one that is easiest to
/// misread: it does not mean the client is misconfigured, it means the stored
/// refresh token is no longer accepted — normally because the credentials were
/// copied from a machine that has since refreshed them and rotated the token
/// out from under this copy. The fix is to import them again, so the message
/// says that instead of leaving it to be inferred.
#[must_use]
pub fn describe_oauth_error(status: u16, raw: &str) -> String {
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

/// Unix milliseconds now.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The refresh request body, per the bundle's `m1()`.
#[derive(Debug, Serialize)]
pub struct RefreshRequest<'a> {
    pub grant_type: &'static str,
    pub refresh_token: &'a str,
    pub client_id: &'static str,
    /// Space-joined [`REFRESH_SCOPES`]. Claude Code sends this; omitting it
    /// asks the server to guess, and what it guesses is not documented.
    pub scope: String,
}

impl<'a> RefreshRequest<'a> {
    #[must_use]
    pub fn new(refresh_token: &'a str) -> Self {
        Self {
            grant_type: "refresh_token",
            refresh_token,
            client_id: CLIENT_ID,
            scope: REFRESH_SCOPES.join(" "),
        }
    }
}

/// The refresh response, of which we keep everything that survives a restart.
#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds. We compute an absolute deadline ourselves, because a duration
    /// is meaningless once it has been written to disk.
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl RefreshResponse {
    /// Fold a refresh response into a storable blob.
    #[must_use]
    pub fn into_blob(self) -> OauthBlob {
        OauthBlob {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: now_ms() + self.expires_in * 1000,
            scopes: self
                .scope
                .as_deref()
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Transport (ureq)
// ---------------------------------------------------------------------------

#[cfg(feature = "ureq")]
mod transport {
    use std::time::Duration;

    use super::{
        ANTHROPIC_BETA, ANTHROPIC_VERSION, Identity, OauthBlob, PROFILE_PATH, RefreshRequest, RefreshResponse,
        TOKEN_URL, USAGE_PATH, api_headers, describe_oauth_error, oauth_headers, parse_profile,
    };

    /// A ureq agent for Anthropic calls.
    ///
    /// **No global timeout** (LLM streams run for minutes) and default `WebPKI`
    /// verification. A connect timeout still bounds a dead upstream.
    ///
    /// `http_status_as_error(false)`: a relay must be transparent to the
    /// upstream's status. ureq's default turns any non-2xx into `Err`, which
    /// would collapse a real upstream 429/500/529 — with its explanatory body —
    /// into an opaque synthetic 502, so the caller can neither see the reason
    /// nor honour `Retry-After`. With this off, non-2xx arrives as `Ok(resp)`
    /// and the true status and body stream through.
    ///
    /// The User-Agent is set per-request rather than on the agent, because the
    /// inference path and the OAuth path send different ones.
    #[must_use]
    pub fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build()
            .new_agent()
    }

    /// Exchange a refresh token for a fresh blob.
    ///
    /// # Errors
    /// The request failed, or the server refused the grant — in which case the
    /// message is [`describe_oauth_error`]'s, not serde's.
    pub fn refresh(refresh_token: &str) -> Result<OauthBlob, String> {
        let mut rb = agent().post(TOKEN_URL);
        for (k, v) in oauth_headers() {
            rb = rb.header(k, &v);
        }
        let mut resp = rb
            .send_json(RefreshRequest::new(refresh_token))
            .map_err(|e| format!("refresh request failed: {e}"))?;
        // The agent does NOT raise HTTP errors, so a rejected refresh arrives
        // here as a normal response carrying an OAuth error envelope. Decoding
        // that as a token blob reports "missing field `access_token`" — which
        // describes our struct, not the problem, and sent an operator looking
        // at the wrong thing. Read the status first and say what the server
        // actually said.
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let raw = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(format!("refresh rejected: {}", describe_oauth_error(status, &raw)));
        }
        let body: RefreshResponse = resp
            .body_mut()
            .read_json()
            .map_err(|e| format!("refresh decode: {e}"))?;
        Ok(body.into_blob())
    }

    /// Ask Anthropic who a token belongs to.
    ///
    /// Doubles as a liveness check: a revoked or expired token cannot answer,
    /// so a caller that gets an [`Identity`] back knows the credential works
    /// AND whose it is, in one round trip.
    ///
    /// # Errors
    /// The request failed, upstream refused the token, or the body was not the
    /// profile shape.
    pub fn profile(base: &str, access_token: &str) -> Result<Identity, String> {
        let mut rb = agent().get(&format!("{base}{PROFILE_PATH}"));
        for (k, v) in oauth_headers() {
            rb = rb.header(k, &v);
        }
        let mut resp = rb
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("anthropic-beta", super::OAUTH_BETA)
            // Claude Code sends this: a cached profile would defeat the point,
            // which is to ask upstream whether the token is alive right now.
            .header("Cache-Control", "no-cache")
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
        parse_profile(&body)
    }

    /// Ask Anthropic how much of the plan has been used.
    ///
    /// Returns the raw status and body: utilisation parsing belongs to the
    /// caller, and a non-2xx body is the only place upstream says why.
    ///
    /// # Errors
    /// The request could not be sent, or its body could not be read.
    pub fn usage(base: &str, access_token: &str) -> Result<(u16, String), String> {
        let mut rb = agent().get(&format!("{base}{USAGE_PATH}"));
        for (k, v) in api_headers(None) {
            rb = rb.header(k, &v);
        }
        let mut resp = rb
            .header("Authorization", &format!("Bearer {access_token}"))
            // Without this beta flag the endpoint refuses an OAuth credential.
            .header("anthropic-beta", super::OAUTH_BETA)
            .header("Content-Type", "application/json")
            .call()
            .map_err(|e| format!("usage request failed: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("usage body: {e}"))?;
        Ok((status, body))
    }

    /// A one-token completion, for timing the real round trip.
    ///
    /// # Errors
    /// The request could not be sent.
    pub fn ping_messages(base: &str, access_token: &str, model: &str) -> Result<(u16, String), String> {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}],
        });
        let mut rb = agent().post(&format!("{base}{}", super::MESSAGES_PATH));
        for (k, v) in api_headers(None) {
            rb = rb.header(k, &v);
        }
        let mut resp = rb
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| format!("{e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }
}

#[cfg(feature = "ureq")]
pub use transport::{agent, ping_messages, profile, refresh, usage};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_matches_the_grammar_claude_code_uses() {
        // Captured off the wire from claude 2.1.266 (`claude -p`, which sets
        // the entrypoint to sdk-cli):
        //   claude-cli/2.1.266 (external, sdk-cli)
        // The default entrypoint is `cli`. Anything else here means Anthropic
        // sees a client it does not recognise on the OAuth path.
        // SAFETY-of-test: these vars are read, not written, by user_agent().
        let ua = user_agent();
        assert!(ua.starts_with("claude-cli/"), "{ua}");
        assert!(ua.contains("(external, "), "{ua}");
        assert!(ua.ends_with(')'), "{ua}");
        assert!(ua.contains(CLAUDE_CODE_VERSION), "{ua}");
    }

    #[test]
    fn the_api_header_set_is_the_one_claude_code_sends() {
        let h = api_headers(Some("fdb378a6-aab8-4cd3-ba82-82c9a7248507"));
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        // Confirmed against a captured request: x-app is `cli`, the session id
        // travels, and the API contract is the dated one.
        assert_eq!(get("x-app"), Some("cli"));
        assert_eq!(get("anthropic-version"), Some("2023-06-01"));
        assert_eq!(
            get("X-Claude-Code-Session-Id"),
            Some("fdb378a6-aab8-4cd3-ba82-82c9a7248507")
        );
        assert!(get("User-Agent").is_some_and(|u| u.starts_with("claude-cli/")));
        // Credentials are the caller's business — this set must never carry one.
        assert!(get("Authorization").is_none());
        assert!(get("x-api-key").is_none());
    }

    #[test]
    fn the_oauth_path_sends_no_inference_identity_headers() {
        // The token/profile/roles calls are plain axios requests upstream:
        // no x-app, no session id. Sending them would make our OAuth traffic
        // distinguishable from Claude Code's in the one place it matters.
        let h = oauth_headers();
        assert!(h.iter().all(|(k, _)| *k != "x-app"));
        assert!(h.iter().all(|(k, _)| *k != "X-Claude-Code-Session-Id"));
        assert!(h.iter().any(|(k, v)| *k == "Content-Type" && v == "application/json"));
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

        // A real client sends ten flags; all of them survive.
        let real = "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
                    thinking-token-count-2026-05-13,context-management-2025-06-27,\
                    prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                    effort-2025-11-24,fallback-credit-2026-06-01,afk-mode-2026-01-31";
        let merged = merge_beta(Some(real), ANTHROPIC_BETA);
        for flag in real.split(',') {
            assert!(merged.contains(flag), "{flag} dropped");
        }
        assert_eq!(merged.matches("claude-code-20250219").count(), 1, "{merged}");
    }

    #[test]
    fn parses_the_claude_credentials_schema() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","refreshToken":"sk-ant-ort01-def","expiresAt":1748276587173,"scopes":["user:inference"]}}"#;
        let blob = parse_credentials(raw).expect("parse");
        assert_eq!(blob.access_token, "sk-ant-oat01-abc");
        assert_eq!(blob.refresh_token, "sk-ant-ort01-def");
        assert_eq!(blob.expires_at, 1_748_276_587_173);
        // One scope short of what /api/oauth/usage needs — the plan monitor
        // must not conclude "the API is broken" from a credential that was
        // never allowed to ask.
        assert!(!blob.can_read_usage());
    }

    #[test]
    fn missing_oauth_key_is_an_error() {
        assert!(parse_credentials(r#"{"nope":true}"#).is_err());
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials("{}").is_err());
    }

    #[test]
    fn a_refresh_body_carries_the_scopes_claude_code_asks_for() {
        let json = serde_json::to_value(RefreshRequest::new("rt-x")).expect("serialise");
        assert_eq!(json["grant_type"], "refresh_token");
        assert_eq!(json["client_id"], CLIENT_ID);
        // org:create_api_key is a login-time consent, not a standing capability
        // — asking for it on every refresh would widen the token for no reason.
        let scope = json["scope"].as_str().unwrap_or_default();
        assert!(scope.contains("user:inference"), "{scope}");
        assert!(scope.contains("user:profile"), "{scope}");
        assert!(!scope.contains("org:create_api_key"), "{scope}");
    }

    #[test]
    fn an_expiring_token_is_refreshed_before_it_expires_not_after() {
        // The lead time is the point: a token that expires while a request is
        // in flight fails that request, and the caller cannot tell that from a
        // revoked credential.
        let blob = |expires_at| OauthBlob {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            expires_at,
            scopes: vec!["user:inference".to_owned(), "user:profile".to_owned()],
        };
        assert!(blob(0).needs_refresh(), "an expired token must refresh");
        assert!(
            blob(now_ms() + REFRESH_LEAD_MS / 2).needs_refresh(),
            "inside the lead window it must refresh"
        );
        assert!(
            !blob(now_ms() + REFRESH_LEAD_MS * 10).needs_refresh(),
            "a fresh token must not refresh"
        );
        assert!(blob(u64::MAX).can_read_usage());
    }

    #[test]
    fn an_oauth_rejection_names_the_thing_the_operator_must_do() {
        // Real body from platform.claude.com, captured with a bogus token.
        let msg = describe_oauth_error(
            400,
            r#"{"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}"#,
        );
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(msg.contains("Refresh token not found or invalid"), "{msg}");
        assert!(msg.contains("import the credentials again"), "{msg}");
        // A non-envelope body must not be discarded: it is the only evidence.
        let msg = describe_oauth_error(502, "<html>gateway</html>");
        assert!(msg.contains("502"), "{msg}");
        assert!(msg.contains("gateway"), "{msg}");
    }

    #[test]
    fn a_profile_without_an_account_uuid_is_refused() {
        // The uuid is the only field the same-account guard can rest on, so an
        // absent one must be an error rather than an empty string that
        // compares equal to another empty string.
        assert!(parse_profile(r#"{"account":{"email":"a@b.c"}}"#).is_err());
        assert!(parse_profile("not json").is_err());
        let id = parse_profile(r#"{"account":{"uuid":"u-1","email":"a@b.c"},"organization":{"uuid":"o-1"}}"#)
            .expect("well-formed profile");
        assert_eq!(id.account_uuid, "u-1");
        assert_eq!(id.organization_uuid, "o-1");
        assert_eq!(id.email, "a@b.c");
    }

    #[test]
    fn credentials_are_written_unreadable_to_anyone_else() {
        let dir = std::env::temp_dir().join(format!("claude-api-persist-{}", std::process::id()));
        let path = dir.join(".claude").join(".credentials.json");
        let _ = std::fs::remove_dir_all(&dir);

        let blob = OauthBlob {
            access_token: "sk-ant-oat01-live".to_owned(),
            refresh_token: "rt-live".to_owned(),
            expires_at: 9_000_000_000_000,
            scopes: vec!["user:inference".to_owned()],
        };
        persist_credentials(&path, &blob).expect("write");

        // Round-trips through the on-disk shape Claude Code itself reads.
        let back = parse_credentials(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back.access_token, "sk-ant-oat01-live");
        assert_eq!(back.refresh_token, "rt-live");
        // The parent is created — a fresh proxy host has no ~/.claude at all.
        assert!(path.parent().is_some_and(Path::is_dir));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a live refresh token must not be world-readable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
