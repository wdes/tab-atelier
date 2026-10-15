// SPDX-License-Identifier: MPL-2.0

//! Runtime configuration.
//!
//! The Kiosk is a multi-remote aggregator: there is no single daemon it talks
//! to, only the remotes the operator configures under Serveurs, each with its
//! own url and token. So there is no `upstream` here — a central one would be a
//! single daemon by another name, and the whole point is that there is none.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::decisions::Journal;
use crate::intent::{self, Intentions};
use crate::remotes::Remotes;

/// Address the kiosk listens on when `TAB_ATELIER_KIOSK_ADDR` is unset.
///
/// All interfaces, deliberately: the Kiosk exists to be looked at from another
/// machine as much as this one. The token is what gates it, and the package
/// generates one on first start (the panes fail closed without it), so an open
/// port is an authenticated door rather than an open one. Set
/// `TAB_ATELIER_KIOSK_ADDR=127.0.0.1:8282` to keep it loopback-only.
pub const DEFAULT_BIND: &str = "0.0.0.0:8282";

/// Where the kiosk's own data lives when the state-dir knobs are unset.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/tab-atelier-kiosk";

/// Runtime knobs, read from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on.
    pub bind: SocketAddr,
    /// The decision journal this crate serves and mutates.
    pub journal: Journal,
    /// The token the panes require, or `None` for a read-refusing kiosk.
    ///
    /// `None` is fail-closed: the panes answer 401 rather than serving an
    /// unauthenticated door onto the board. A kiosk meant to be looked at gets a
    /// token; one that is not, does not.
    pub token: Option<String>,
    /// Where the token was read from or written to, when it is file-backed.
    ///
    /// `Some` when the token came from the token file or was generated into it,
    /// `None` when it came from the environment or there is none. The server logs
    /// this path at startup so an operator knows what to open the board with
    /// without it ever being printed.
    pub token_file: Option<PathBuf>,
    /// The servers this Kiosk aggregates.
    ///
    /// The Kiosk's point is to take advantage of several resources at once, so
    /// the tab manager reaches a *set* of daemons rather than one. The list and
    /// its tokens live here, server-side; the browser sets them but never holds
    /// them.
    pub remotes: Remotes,
    /// The intentions the Kiosk holds — its own data, like the board.
    pub intentions: Intentions,
}

impl Config {
    /// Validate the knobs, from the environment or the command line.
    ///
    /// # Errors
    ///
    /// Fails when the bind address is unparseable.
    pub fn new(bind: &str) -> Result<Self, String> {
        let bind = bind
            .parse()
            .map_err(|err| format!("bind address {bind:?} is not an address: {err}"))?;
        let state = std::env::var("TAB_ATELIER_KIOSK_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string());
        let journal = std::env::var("TAB_ATELIER_DECISIONS")
            .map_or_else(|_| PathBuf::from(&state).join("decisions.jsonl"), PathBuf::from);
        let outbox =
            std::env::var("TAB_ATELIER_OUTBOX").map_or_else(|_| PathBuf::from(&state).join("outbox"), PathBuf::from);
        let state_dir = PathBuf::from(&state);
        let remotes =
            std::env::var("TAB_ATELIER_REMOTES").map_or_else(|_| state_dir.join("remotes.json"), PathBuf::from);
        let (token, token_file) = resolve_token(&state_dir, std::env::var("TAB_ATELIER_KIOSK_TOKEN").ok());
        let intentions_dir = std::env::var("TAB_ATELIER_INTENTIONS")
            .map_or_else(|_| state_dir.join(intent::DEFAULT_INTENTIONS_DIR), PathBuf::from);
        let intentions = Intentions::new(intentions_dir).map_err(|e| format!("intentions: {e}"))?;
        Ok(Self {
            bind,
            journal: Journal { path: journal, outbox },
            token,
            token_file,
            remotes: Remotes { path: remotes },
            intentions,
        })
    }

    /// Read `TAB_ATELIER_KIOSK_ADDR`, falling back to [`DEFAULT_BIND`].
    ///
    /// # Errors
    ///
    /// As [`Config::new`].
    pub fn from_env() -> Result<Self, String> {
        let bind = std::env::var("TAB_ATELIER_KIOSK_ADDR").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        Self::new(&bind)
    }
}

/// Resolves the pane token: the environment wins, then a token file under the
/// state directory, and failing both, generate one into that file.
///
/// This is what gives the kiosk an *initial* token rather than none: an
/// operator who never sets one would otherwise face a board that answers 401
/// with no way to open it. Generating it here means the first start leaves a
/// token behind to read, and every start after reuses it, so a restart does not
/// silently rotate the secret out from under a bookmarked URL.
///
/// Generation fails soft. A state directory the service user cannot write means
/// no token, which is the fail-closed default rather than a start failure — a
/// read-only deployment is a deployment that chose not to open the board.
fn resolve_token(state_dir: &Path, from_env: Option<String>) -> (Option<String>, Option<PathBuf>) {
    if let Some(token) = from_env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return (Some(token), None);
    }
    let path = state_dir.join("token");
    if let Ok(existing) = fs::read_to_string(&path) {
        let token = existing.trim().to_string();
        if !token.is_empty() {
            return (Some(token), Some(path));
        }
    }
    match generate_token(&path) {
        Ok(token) => (Some(token), Some(path)),
        Err(err) => {
            eprintln!("kiosk: no token and could not write {}: {err}", path.display());
            (None, None)
        }
    }
}

/// Writes a fresh 32-byte token to `path`, `0600`, and returns it.
fn generate_token(path: &Path) -> std::io::Result<String> {
    let token = random_hex(32)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, &token)?;
    set_owner_only(path)?;
    Ok(token)
}

/// `bytes` random bytes from the OS, hex-encoded.
///
/// Used for the pane token and for remote ids. Both are identifiers that must
/// not collide or be guessable, which `/dev/urandom` gives and no clock does.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut buf = vec![0_u8; bytes];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let hex = buf.iter().fold(String::with_capacity(bytes * 2), |mut acc, byte| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{byte:02x}");
        acc
    });
    Ok(hex)
}

/// Restricts a file to its owner, so a token is not world-readable.
#[cfg(unix)]
pub fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// No-op off unix, where the kiosk does not run as a service.
#[cfg(not(unix))]
pub fn set_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Deadlines for the hop to a remote's daemon, one per stage that must produce
/// something. Used by the [`crate::daemon`] client.
#[derive(Debug, Clone, Copy)]
pub struct HopTimeouts {
    pub resolve: Option<Duration>,
    pub connect: Option<Duration>,
    pub send_request: Option<Duration>,
    pub send_body: Option<Duration>,
    /// Deadline for the RESPONSE HEADERS to arrive.
    ///
    /// `None` on purpose, and it must stay that way. ureq keeps this stage armed
    /// while the body is being read: its deadline is fixed at the moment the
    /// headers landed, so a non-`None` value caps the TOTAL life of the body,
    /// killing a healthy stream at a fixed age. Waiting for headers is still
    /// bounded — the `send_request` deadline covers that window.
    pub recv_response: Option<Duration>,
    /// Idle deadline on the response body: a stream that keeps producing
    /// survives however long it likes, one that goes quiet this long is dead.
    /// This, not `recv_response`, is what bounds a body.
    pub recv_body: Option<Duration>,
}

impl Default for HopTimeouts {
    fn default() -> Self {
        Self {
            resolve: Some(Duration::from_secs(5)),
            connect: Some(Duration::from_secs(10)),
            send_request: Some(Duration::from_secs(10)),
            send_body: Some(Duration::from_secs(30)),
            recv_response: None,
            recv_body: Some(Duration::from_mins(5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{generate_token, resolve_token};
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make state dir");
        dir
    }

    /// A first start generates and stores a token; a restart reuses it rather
    /// than rotating the secret out from under a bookmarked URL.
    #[test]
    fn the_token_is_generated_once_and_reused() {
        let dir = scratch("token");
        let (token, path) = resolve_token(&dir, None);
        let token = token.expect("a token is generated");
        let path = path.expect("the token is file-backed");
        assert_eq!(token.len(), 64, "32 bytes of hex");
        assert_eq!(fs::read_to_string(&path).expect("read token").trim(), token);

        let (again, _) = resolve_token(&dir, None);
        assert_eq!(again.as_deref(), Some(token.as_str()), "the second start reuses it");
    }

    /// The environment wins, and its token is not written to disk.
    #[test]
    fn the_environment_token_wins_and_is_not_stored() {
        let dir = scratch("env");
        let (token, path) = resolve_token(&dir, Some("from-env".to_string()));
        assert_eq!(token.as_deref(), Some("from-env"));
        assert!(path.is_none(), "an env token is not written to the state dir");
        assert!(!dir.join("token").exists());
    }

    /// An unwritable state directory yields no token, not a crash — the panes
    /// then fail closed, which is the right answer for a read-only deployment.
    #[test]
    fn an_unwritable_state_dir_fails_soft() {
        let (token, path) = resolve_token(&PathBuf::from("/proc/nonexistent/ta-kiosk"), None);
        assert!(token.is_none());
        assert!(path.is_none());
    }

    /// The generated file is owner-only.
    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("mode");
        let path = dir.join("token");
        generate_token(&path).expect("generate");
        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a token file is not world-readable");
    }
}
