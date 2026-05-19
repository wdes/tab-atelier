// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read + refresh Claude Code's OAuth credentials.
//!
//! Claude Code on Linux stores its OAuth blob in
//! `~/.claude/.credentials.json` (mode 0600). The schema is:
//!
//! ```json
//! { "claudeAiOauth": {
//!     "accessToken":  "sk-ant-oat01-...",
//!     "refreshToken": "sk-ant-ort01-...",
//!     "expiresAt":    1748276587173,    // unix-ms
//!     "scopes":       ["user:inference", "user:profile"]
//! }}
//! ```
//!
//! Access tokens live ~8 h. We refresh them ourselves when they're
//! within 60 s of expiry. Each refresh rotates `refreshToken` so we
//! must persist the new blob back to disk atomically.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// Refresh this many ms before the access token actually expires so
/// an in-flight request can't race the rollover.
const REFRESH_LEAD_MS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no $HOME — can't locate ~/.claude/.credentials.json")]
    NoHome,
    #[error("credentials file is missing: {0}")]
    Missing(PathBuf),
    #[error("credentials file is malformed: {0}")]
    Malformed(serde_json::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("refresh request failed: {0}")]
    Refresh(String),
}

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

/// Caller-facing handle. Hold one across the session; call
/// `access_token().await` before every API request — it transparently
/// refreshes when needed.
pub struct Auth {
    path: PathBuf,
    blob: tokio::sync::Mutex<OauthBlob>,
    http: reqwest::Client,
}

/// Load the credential file (no I/O on the auth endpoint yet).
pub fn load() -> Result<Auth, AuthError> {
    let home = std::env::var_os("HOME").ok_or(AuthError::NoHome)?;
    let path = PathBuf::from(home).join(".claude").join(".credentials.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AuthError::Missing(path.clone())
        } else {
            AuthError::Io(e)
        }
    })?;
    let parsed: CredentialsFile = serde_json::from_str(&raw).map_err(AuthError::Malformed)?;
    Ok(Auth {
        path,
        blob: tokio::sync::Mutex::new(parsed.claude_ai_oauth),
        http: reqwest::Client::builder()
            .user_agent("catbus-agent/0.1 (tab-atelier)")
            .build()
            .expect("http client init"),
    })
}

impl Auth {
    /// Get the current access token, refreshing first when it's
    /// within `REFRESH_LEAD_MS` of expiry.
    pub async fn access_token(&self) -> Result<String, AuthError> {
        let mut blob = self.blob.lock().await;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        if blob.expires_at > now_ms + REFRESH_LEAD_MS {
            return Ok(blob.access_token.clone());
        }
        log::info!("refreshing OAuth token (expires_at={})", blob.expires_at);
        let refreshed = self.do_refresh(&blob.refresh_token).await?;
        *blob = refreshed;
        // Persist atomically: write a temp file + rename. This
        // matters because the refresh token rotates on every use; a
        // partial write would brick auth.
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&CredentialsFile {
            claude_ai_oauth: blob.clone(),
        })
        .expect("serializable blob");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(blob.access_token.clone())
    }

    async fn do_refresh(&self, refresh_token: &str) -> Result<OauthBlob, AuthError> {
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
            // Anthropic returns expires_in (seconds) — we compute
            // an absolute deadline ourselves.
            expires_in: u64,
            #[serde(default)]
            scope: Option<String>,
        }
        let resp = self
            .http
            .post(REFRESH_URL)
            .json(&Req {
                grant_type: "refresh_token",
                refresh_token,
                client_id: CLIENT_ID,
            })
            .send()
            .await
            .map_err(|e| AuthError::Refresh(e.to_string()))?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AuthError::Refresh(format!("{code}: {body}")));
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| AuthError::Refresh(format!("decode: {e}")))?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Ok(OauthBlob {
            access_token: body.access_token,
            refresh_token: body.refresh_token,
            expires_at: now_ms + body.expires_in * 1000,
            scopes: body
                .scope
                .as_deref()
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }
}
