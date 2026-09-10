// SPDX-License-Identifier: MPL-2.0

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

use claude_api::{CredentialsFile, OauthBlob, RefreshRequest, TOKEN_URL};

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
        // Claude Code's own User-Agent: this credential is a Claude Code
        // OAuth token, and Anthropic's OAuth path is for Claude Code.
        http: reqwest::Client::builder()
            .user_agent(claude_api::user_agent())
            .build()
            .expect("http client init"),
    })
}

impl Auth {
    /// Get the current access token, refreshing first when it's
    /// within `REFRESH_LEAD_MS` of expiry.
    pub async fn access_token(&self) -> Result<String, AuthError> {
        let mut blob = self.blob.lock().await;
        if !blob.needs_refresh() {
            return Ok(blob.access_token.clone());
        }
        log::info!("refreshing OAuth token (expires_at={})", blob.expires_at);
        let refreshed = self.do_refresh(&blob.refresh_token).await?;
        *blob = refreshed;
        // Persist atomically and 0600-from-creation: the refresh token
        // rotates on every use, so a partial write would brick auth, and
        // this copy used to be chmod'ed only after a 0644 create.
        claude_api::persist_credentials(&self.path, &blob).map_err(AuthError::Refresh)?;
        Ok(blob.access_token.clone())
    }

    async fn do_refresh(&self, refresh_token: &str) -> Result<OauthBlob, AuthError> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .json(&RefreshRequest::new(refresh_token))
            .send()
            .await
            .map_err(|e| AuthError::Refresh(e.to_string()))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            // Says WHICH thing to fix — `invalid_grant` means the stored
            // refresh token was rotated out from under this copy, not that
            // anything here is misconfigured.
            return Err(AuthError::Refresh(claude_api::describe_oauth_error(code, &body)));
        }
        let body: claude_api::RefreshResponse = resp
            .json()
            .await
            .map_err(|e| AuthError::Refresh(format!("decode: {e}")))?;
        Ok(body.into_blob())
    }
}
