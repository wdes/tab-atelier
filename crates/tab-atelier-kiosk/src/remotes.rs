// SPDX-License-Identifier: MPL-2.0

//! The servers this Kiosk aggregates.
//!
//! The Kiosk's whole point is to take advantage of several resources at once, so
//! it holds a list of remotes — each a daemon it can reach, with the token that
//! opens it. The list lives here, on the Kiosk (the state directory, `0600`): the
//! settings screen sets it, but the browser never holds a remote's token, and the
//! Kiosk reaches each server server-side. A token in the browser would be one
//! cross-site script away from every fleet the operator owns.
//!
//! This mirrors the desktop app's `RemoteEndpoint` (`tab-atelier remote add`),
//! narrowed to what the Kiosk needs: a label, a URL, a token. TLS pinning and
//! Cloudflare Access stay the app's business until the Kiosk has to reach a
//! remote behind them.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::random_hex;

/// One server the Kiosk can reach.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Remote {
    /// Stable handle, generated on add. The settings screen addresses a remote
    /// by this, so renaming the label does not break a link.
    pub id: String,
    /// Short human label ("build-box", "colossus").
    pub label: String,
    /// Base URL of the remote's API: `http://host:port` or `https://host:port`.
    pub url: String,
    /// Bearer token for that server. Never leaves the Kiosk.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

/// A remote as the settings screen sees it: everything but the token.
///
/// The token is needed to reach the server, not to list the server, so the list
/// route hands back this and the browser never receives a secret it does not
/// already own.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteView {
    /// See [`Remote::id`].
    pub id: String,
    /// See [`Remote::label`].
    pub label: String,
    /// See [`Remote::url`].
    pub url: String,
    /// Whether a token is set, without disclosing it.
    pub has_token: bool,
}

impl From<&Remote> for RemoteView {
    fn from(remote: &Remote) -> Self {
        Self {
            id: remote.id.clone(),
            label: remote.label.clone(),
            url: remote.url.clone(),
            has_token: !remote.token.is_empty(),
        }
    }
}

/// Why a remote could not be added, or the list could not be read or written.
#[derive(Debug, PartialEq, Eq)]
pub enum RemoteError {
    /// A field is missing or malformed.
    Invalid(String),
    /// No remote carries that id.
    NotFound,
    /// The list file cannot be read or written.
    Unavailable,
}

/// The remote list on disk.
#[derive(Debug, Clone)]
pub struct Remotes {
    /// The JSON file holding the list.
    pub path: PathBuf,
}

impl Remotes {
    /// The configured remotes, in the order they were added.
    ///
    /// A missing file is an empty list: a fresh Kiosk has no servers yet, which
    /// is a state and not a fault.
    ///
    /// # Errors
    ///
    /// [`RemoteError::Unavailable`] when the file exists but cannot be read or
    /// parsed.
    pub fn list(&self) -> Result<Vec<Remote>, RemoteError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(Vec::new()),
            Ok(raw) => serde_json::from_str(&raw).map_err(|_| RemoteError::Unavailable),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(_) => Err(RemoteError::Unavailable),
        }
    }

    /// One remote by id.
    ///
    /// # Errors
    ///
    /// [`RemoteError::NotFound`] when no remote carries the id.
    pub fn get(&self, id: &str) -> Result<Remote, RemoteError> {
        self.list()?
            .into_iter()
            .find(|remote| remote.id == id)
            .ok_or(RemoteError::NotFound)
    }

    /// Adds a remote, returning it (with its generated id).
    ///
    /// # Errors
    ///
    /// [`RemoteError::Invalid`] for a missing label, a URL that is not http(s),
    /// or a duplicate label — two servers under one name is a list the operator
    /// cannot read.
    pub fn add(&self, label: &str, url: &str, token: &str) -> Result<Remote, RemoteError> {
        let label = label.trim();
        let url = url.trim().trim_end_matches('/');
        if label.is_empty() {
            return Err(RemoteError::Invalid("a label is required".to_string()));
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(RemoteError::Invalid(format!(
                "url {url:?} must start with http:// or https://"
            )));
        }
        let mut remotes = self.list()?;
        if remotes.iter().any(|remote| remote.label.eq_ignore_ascii_case(label)) {
            return Err(RemoteError::Invalid(format!(
                "a remote labelled {label:?} already exists"
            )));
        }
        let remote = Remote {
            id: random_hex(8).map_err(|_| RemoteError::Unavailable)?,
            label: label.to_string(),
            url: url.to_string(),
            token: token.trim().to_string(),
        };
        remotes.push(remote.clone());
        self.save(&remotes)?;
        Ok(remote)
    }

    /// Removes a remote by id.
    ///
    /// # Errors
    ///
    /// [`RemoteError::NotFound`] when no remote carries the id.
    pub fn remove(&self, id: &str) -> Result<Remote, RemoteError> {
        let mut remotes = self.list()?;
        let index = remotes
            .iter()
            .position(|remote| remote.id == id)
            .ok_or(RemoteError::NotFound)?;
        let removed = remotes.remove(index);
        self.save(&remotes)?;
        Ok(removed)
    }

    /// Replaces the stored token for a remote — for rotating a credential
    /// without removing and re-adding the server.
    ///
    /// # Errors
    ///
    /// [`RemoteError::NotFound`] when no remote carries the id.
    pub fn set_token(&self, id: &str, token: &str) -> Result<Remote, RemoteError> {
        let mut remotes = self.list()?;
        let remote = remotes
            .iter_mut()
            .find(|remote| remote.id == id)
            .ok_or(RemoteError::NotFound)?;
        remote.token = token.trim().to_string();
        let updated = remote.clone();
        self.save(&remotes)?;
        Ok(updated)
    }

    /// Writes the whole list, `0600` — it holds tokens.
    fn save(&self, remotes: &[Remote]) -> Result<(), RemoteError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|_| RemoteError::Unavailable)?;
        }
        let json = serde_json::to_string_pretty(remotes).map_err(|_| RemoteError::Unavailable)?;
        fs::write(&self.path, json).map_err(|_| RemoteError::Unavailable)?;
        crate::config::set_owner_only(&self.path).map_err(|_| RemoteError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::{Remote, RemoteError, Remotes};
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-remotes-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make state dir");
        dir
    }

    fn store(name: &str) -> Remotes {
        Remotes {
            path: scratch(name).join("remotes.json"),
        }
    }

    /// A fresh store is an empty list, not an error.
    #[test]
    fn a_missing_file_is_an_empty_list() {
        assert!(store("empty").list().expect("list").is_empty());
    }

    /// Adding gives a stable id, a trimmed url, and a persisted record; the
    /// order is the order added.
    #[test]
    fn add_persists_and_generates_an_id() {
        let remotes = store("add");
        let a = remotes
            .add("build-box", "http://10.0.0.1:7890/", "tok-a")
            .expect("add a");
        assert_eq!(a.url, "http://10.0.0.1:7890", "a trailing slash is trimmed");
        assert_eq!(a.id.len(), 16, "8 bytes of hex");
        let b = remotes
            .add("colossus", "https://box.example:7890", "tok-b")
            .expect("add b");
        assert_ne!(a.id, b.id);

        let listed = remotes.list().expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].label, "build-box", "added order is kept");
        assert_eq!(remotes.get(&b.id).expect("get b").token, "tok-b");
    }

    /// A label is required, the url must be http(s), and two remotes cannot
    /// share a label.
    #[test]
    fn add_validates() {
        let remotes = store("validate");
        assert!(matches!(
            remotes.add("", "http://x:1", "t"),
            Err(RemoteError::Invalid(_))
        ));
        assert!(matches!(
            remotes.add("a", "ftp://x:1", "t"),
            Err(RemoteError::Invalid(_))
        ));
        remotes.add("a", "http://x:1", "t").expect("add a");
        assert!(
            matches!(remotes.add("A", "http://y:1", "t"), Err(RemoteError::Invalid(_))),
            "labels are unique case-insensitively"
        );
    }

    /// Removing takes one out and leaves the rest; an unknown id is not found.
    #[test]
    fn remove_takes_one() {
        let remotes = store("remove");
        let a = remotes.add("a", "http://a:1", "t").expect("add");
        remotes.add("b", "http://b:1", "t").expect("add");
        let removed = remotes.remove(&a.id).expect("remove");
        assert_eq!(removed.label, "a");
        let left = remotes.list().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].label, "b");
        assert_eq!(remotes.remove("nope"), Err(RemoteError::NotFound));
    }

    /// A token can be rotated without touching the rest of the record.
    #[test]
    fn a_token_can_be_rotated() {
        let remotes = store("rotate");
        let a = remotes.add("a", "http://a:1", "old").expect("add");
        let updated = remotes.set_token(&a.id, "new").expect("rotate");
        assert_eq!(updated.token, "new");
        assert_eq!(updated.label, "a");
        assert_eq!(remotes.get(&a.id).expect("get").token, "new");
        assert_eq!(remotes.set_token("nope", "x"), Err(RemoteError::NotFound));
    }

    /// The list view never carries the token.
    #[test]
    fn the_view_redacts_the_token() {
        let remote = Remote {
            id: "i".to_string(),
            label: "l".to_string(),
            url: "http://x:1".to_string(),
            token: "secret".to_string(),
        };
        let view = super::RemoteView::from(&remote);
        assert!(view.has_token);
        let json = serde_json::to_string(&view).expect("serialize");
        assert!(
            !json.contains("secret"),
            "the token must not be serialisable here: {json}"
        );
    }

    /// The file is owner-only, since it holds tokens.
    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let remotes = store("mode");
        remotes.add("a", "http://a:1", "t").expect("add");
        let mode = fs::metadata(&remotes.path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the token store is not world-readable");
    }
}
