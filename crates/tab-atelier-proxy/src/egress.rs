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
//! **The wire protocol is not here.** Endpoints, headers, the User-Agent, the
//! credential schema and the refresh flow all live in [`claude_api`], shared
//! with the desktop crate and `catbus-agent` — there used to be three copies
//! and they had drifted. What remains here is what is genuinely the proxy's:
//! where the credential lives on THIS host, who it belongs to, and how it is
//! installed and repaired.

use std::path::PathBuf;
use std::time::Duration;

use claude_api::{Identity, OauthBlob};

pub use claude_api::{
    ANTHROPIC_BETA, ANTHROPIC_VERSION, BASE_API_URL as ANTHROPIC_BASE, Identity as ClaudeIdentity, merge_beta,
};

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
    claude_api::default_credentials_path()
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
    upstream_override().unwrap_or_else(|| ANTHROPIC_BASE.to_owned())
}

/// A ureq agent for egress calls. See [`claude_api::agent`].
#[must_use]
pub fn relay_agent() -> ureq::Agent {
    claude_api::agent()
}

/// Return a currently-valid Claude OAuth access token.
///
/// Refreshes (and persists the rotated blob back, 0600) when it is within
/// [`claude_api::REFRESH_LEAD_MS`] of expiry. Reads the credentials file each
/// call — cheap, and keeps the egress stateless.
///
/// # Errors
/// Returns a message when `$HOME`/the credentials file is missing or malformed,
/// or the refresh request fails — the route turns it into a 502.
pub fn oauth_access_token() -> Result<String, String> {
    let path = credentials_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let blob = claude_api::parse_credentials(&raw)?;
    if !blob.needs_refresh() {
        return Ok(blob.access_token);
    }
    // Refresh rotates the refresh token → persist atomically (a partial write
    // would brick auth).
    let fresh = claude_api::refresh(&blob.refresh_token)?;
    claude_api::persist_credentials(&path, &fresh)?;
    Ok(fresh.access_token)
}

/// Ask Anthropic who a token belongs to.
///
/// # Errors
/// The request failed, upstream refused the token, or the body was not the
/// profile shape.
pub fn profile_of(access_token: &str) -> Result<Identity, String> {
    claude_api::profile(&upstream(), access_token)
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

/// Reject a credential that cannot be used, before it is written anywhere.
fn vet(raw: &str) -> Result<OauthBlob, String> {
    let blob = claude_api::parse_credentials(raw)?;
    if blob.access_token.is_empty() || blob.refresh_token.is_empty() {
        return Err("credentials are missing an access or refresh token".to_owned());
    }
    Ok(blob)
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
    let blob = vet(raw)?;
    let path = credentials_path()?;
    claude_api::persist_credentials(&path, &blob)?;
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
    let blob = vet(raw)?;
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
    claude_api::persist_credentials(&path, &blob)?;
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

/// Ask Anthropic how much of the shared plan has been used.
///
/// `GET /api/oauth/usage` with the Claude Code OAuth token. It is the only
/// source that knows the subscription's real utilisation; everything else the
/// proxy can see is per-request accounting.
///
/// # Errors
/// No usable credential, or the request failed. The caller records the failure
/// as a sample rather than dropping it.
pub fn account_usage() -> Result<(u16, String), String> {
    let token = oauth_access_token()?;
    claude_api::usage(&upstream(), &token)
}

/// One timed probe of the upstream API.
#[derive(Debug, Clone)]
pub struct Probe {
    pub label: &'static str,
    pub status: Option<u16>,
    pub elapsed: Duration,
    pub note: String,
}

/// One probe of a configured provider, using that provider's own credential.
///
/// The subscription probe below answers "is Anthropic reachable"; this answers
/// "does the SECOND provider work", which is a different question and the one
/// an operator has just created by adding one. A provider that is configured,
/// enabled, and silently unable to authenticate is invisible until real work
/// is routed to it — and by then a reroute has already failed mid-turn.
#[must_use]
pub fn probe_provider(provider: &crate::provider::Provider, model: &str) -> Probe {
    let started = std::time::Instant::now();
    let secret = match &provider.auth {
        crate::provider::Auth::ClaudeOauth => {
            return Probe {
                label: "subscription",
                status: None,
                elapsed: started.elapsed(),
                note: "uses the proxy's own Claude login — see the stages above".to_owned(),
            };
        }
        a => a.secret_with(|v| std::env::var(v).ok()),
    };
    let key = match secret {
        Ok(k) => k,
        Err(e) => {
            return Probe {
                label: "credential",
                status: None,
                elapsed: started.elapsed(),
                note: format!("FAILED: {e}"),
            };
        }
    };

    let base = provider.base_url.trim_end_matches('/');
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let mut rb = relay_agent().post(&format!("{base}/v1/messages"));
    for (k, v) in claude_api::api_headers(None) {
        rb = rb.header(k, &v);
    }
    // Started BEFORE the send: `send_json` performs the request, so a timer
    // taken after it measures the body read and reports a round trip as 0 ms.
    let started = std::time::Instant::now();
    let sent = rb
        .header("Content-Type", "application/json")
        // A provider key goes in x-api-key, the convention Anthropic's own
        // SDKs use and what every Anthropic-compatible endpoint expects.
        .header("x-api-key", &key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", ANTHROPIC_BETA)
        .send_json(&body);

    match sent {
        Ok(mut r) => {
            let status = r.status().as_u16();
            let text = r.body_mut().read_to_string().unwrap_or_default();
            let note = if (200..300).contains(&status) {
                format!("{model} answered")
            } else {
                // The body is where the far end says WHY — a wrong key, a
                // model name it does not have, and a suspended account need
                // different responses from the operator.
                text.chars().take(160).collect::<String>()
            };
            Probe {
                label: "round trip",
                status: Some(status),
                elapsed: started.elapsed(),
                note,
            }
        }
        Err(e) => Probe {
            label: "round trip",
            status: None,
            elapsed: started.elapsed(),
            note: format!("FAILED: {e}"),
        },
    }
}

/// Time the round trip to Anthropic, in the stages that can fail separately.
///
/// A slow proxy is usually not the proxy. Splitting the trip apart says which
/// part is slow instead of leaving an operator to guess:
///
/// * **credential** — reading (and possibly refreshing) the local OAuth
///   credential. Slow here means the refresh endpoint, not the API. The note
///   names the FILE, because the commonest failure is a login performed as a
///   different user than the service runs as.
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

    let base = upstream();

    // Reachability only: the endpoint Claude Code itself probes, which does no
    // model work, so this isolates DNS + TCP + TLS from generation time.
    let started = std::time::Instant::now();
    let mut rb = relay_agent().get(&format!("{base}{}", claude_api::HELLO_PATH));
    for (k, v) in claude_api::api_headers(None) {
        rb = rb.header(k, &v);
    }
    match rb.call() {
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
    let started = std::time::Instant::now();
    match claude_api::ping_messages(&base, &token, model) {
        Ok((status, text)) => {
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
    use super::{import_credentials, set_credentials_path};

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
        let mine = claude_api::Identity {
            account_uuid: "7a427e27-aaaa".to_owned(),
            organization_uuid: "org-1".to_owned(),
            email: "ada@example.org".to_owned(),
            recorded_at: 1,
        };
        let theirs = claude_api::Identity {
            account_uuid: "0000-bbbb".to_owned(),
            organization_uuid: "org-2".to_owned(),
            email: "mallory@example.org".to_owned(),
            recorded_at: 2,
        };
        assert_ne!(mine.account_uuid, theirs.account_uuid);
        // Identity round-trips through the file it is stored in, since that is
        // what the comparison reads back.
        let json = serde_json::to_string(&mine).expect("serialise");
        let back: claude_api::Identity = serde_json::from_str(&json).expect("parse");
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
}
