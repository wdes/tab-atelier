// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which host answers for which name.
//!
//! Gossip makes every host's *board* the same. It does not make the fleet a
//! federation, because a board is only knowledge — nothing in it decides
//! anything. Two hosts holding identical boards can still hand the same task
//! to two agents, because each was consulting its own lease table.
//!
//! This module is the small amount of ceded sovereignty that fixes it: each
//! task has a **home**, the host it was announced on, and the home host's
//! lease table is authoritative for it. Taking a task announced elsewhere
//! means asking that host — over the `remote` endpoint that already exists for
//! sidecars.
//!
//! The mapping is learned, not configured. A peer's `/blackboard` response
//! names its origin, so a gossip round teaches us which endpoint speaks for
//! which host, and [`remember`] persists it. Nothing has to be registered by
//! hand and no host has to be told the fleet's shape in advance.
//!
//! **When the home host is unreachable, the local table is used instead.** The
//! member keeps working rather than blocking on an absent authority — the
//! price is that a partition can produce two holders, discovered when the
//! second one reports `done`. That is the confederal escape hatch, and it is
//! the right trade when the unit of work is "spend some tokens on a file".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Learned origin → endpoint bindings.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Directory {
    /// Peer origin (their hostname) → the id of our `RemoteEndpoint` for them.
    pub members: BTreeMap<String, String>,
}

fn directory_path() -> std::path::PathBuf {
    crate::platform::state_base_dir()
        .join(crate::APP_DIR)
        .join("federation.json")
}

/// Test/ops override for the directory file.
static PATH_OVERRIDE: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// Point the directory at a different file; `None` restores the default.
pub fn set_directory_path(path: Option<std::path::PathBuf>) {
    if let Ok(mut g) = PATH_OVERRIDE.write() {
        *g = path;
    }
}

fn path() -> std::path::PathBuf {
    PATH_OVERRIDE
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(directory_path)
}

/// Read the learned directory. A missing or unreadable file is an empty
/// directory, not an error: a host that has never gossiped simply knows no
/// peers, which is the correct starting state.
#[must_use]
pub fn load() -> Directory {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record that `origin` is reachable through the endpoint with id
/// `endpoint_id`. Idempotent; a changed binding overwrites (a peer that moved
/// to a new endpoint is the same peer).
pub fn remember(origin: &str, endpoint_id: &str) {
    let origin = origin.trim();
    if origin.is_empty() || endpoint_id.is_empty() {
        return;
    }
    let mut dir = load();
    if dir.members.get(origin).is_some_and(|e| e == endpoint_id) {
        return;
    }
    dir.members.insert(origin.to_owned(), endpoint_id.to_owned());
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string(&dir) {
        let _ = std::fs::write(&p, body);
    }
}

/// The endpoint that speaks for `origin`, if we have learned one and it is
/// still configured.
///
/// Returns `None` for our own origin — the caller uses its local table then,
/// which is the point: a host is its own authority.
#[must_use]
pub fn endpoint_for(origin: &str, me: &str, endpoints: &[crate::RemoteEndpoint]) -> Option<crate::RemoteEndpoint> {
    if origin.is_empty() || origin == me {
        return None;
    }
    let id = load().members.get(origin)?.clone();
    endpoints.iter().find(|e| e.id == id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, label: &str) -> crate::RemoteEndpoint {
        crate::RemoteEndpoint {
            id: id.to_owned(),
            label: label.to_owned(),
            url: format!("http://{label}:7890"),
            token: "t".into(),
            relay_token: String::new(),
            cert_sha256: String::new(),
            cf_access_client_id: String::new(),
            cf_access_client_secret: String::new(),
            autoconnect: false,
        }
    }

    #[test]
    fn the_directory_is_learned_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        set_directory_path(Some(dir.path().join("federation.json")));
        assert!(load().members.is_empty(), "a host that never gossiped knows no peers");

        remember("build-box", "ep-1");
        remember("laptop", "ep-2");
        let eps = [endpoint("ep-1", "build-box"), endpoint("ep-2", "laptop")];

        let found = endpoint_for("build-box", "me", &eps).expect("learned peer");
        assert_eq!(found.label, "build-box");
        // A host is its own authority: never route to yourself.
        assert!(endpoint_for("me", "me", &eps).is_none());
        assert!(endpoint_for("", "me", &eps).is_none());
        // An unknown peer is not an error — we just don't know it yet, and the
        // caller falls back to its own table.
        assert!(endpoint_for("stranger", "me", &eps).is_none());
        // Known origin whose endpoint was deleted: also None, rather than a
        // dangling reference the caller would try to call.
        assert!(endpoint_for("laptop", "me", &eps[..1]).is_none());

        // Re-learning is idempotent; a moved peer rebinds rather than
        // duplicating.
        remember("build-box", "ep-1");
        assert_eq!(load().members.len(), 2);
        remember("build-box", "ep-9");
        assert_eq!(load().members.get("build-box").map(String::as_str), Some("ep-9"));

        // Junk is ignored rather than stored.
        remember("", "ep-3");
        remember("ghost", "");
        assert_eq!(load().members.len(), 2);

        set_directory_path(None);
    }
}
