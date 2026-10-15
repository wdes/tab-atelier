// SPDX-License-Identifier: MPL-2.0

//! Runtime configuration, and the upstream hop it configures.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::decisions::Journal;
use crate::remotes::Remotes;

/// The daemon this crate fronts when `TAB_ATELIER_UPSTREAM` is unset.
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:7890";

/// Address the kiosk listens on when `TAB_ATELIER_KIOSK_ADDR` is unset.
pub const DEFAULT_BIND: &str = "127.0.0.1:8282";

/// Where the kiosk's own data lives when the state-dir knobs are unset.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/tab-atelier-kiosk";

/// Runtime knobs, read from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the daemon to proxy to, no trailing slash.
    pub upstream: String,
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
}

impl Config {
    /// Validate a pair of knobs, from the environment or the command line.
    ///
    /// # Errors
    ///
    /// Fails when the upstream is not an http(s) URL or the bind address is
    /// unparseable.
    pub fn new(upstream: &str, bind: &str) -> Result<Self, String> {
        let upstream = upstream.trim_end_matches('/').to_string();
        if !upstream.starts_with("http://") && !upstream.starts_with("https://") {
            return Err(format!("upstream must be an http(s) URL, got {upstream:?}"));
        }
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
        Ok(Self {
            upstream,
            bind,
            journal: Journal { path: journal, outbox },
            token,
            token_file,
            remotes: Remotes { path: remotes },
        })
    }

    /// Read `TAB_ATELIER_UPSTREAM` and `TAB_ATELIER_KIOSK_ADDR`, falling back
    /// to [`DEFAULT_UPSTREAM`] and [`DEFAULT_BIND`].
    ///
    /// # Errors
    ///
    /// As [`Config::new`].
    pub fn from_env() -> Result<Self, String> {
        let upstream = std::env::var("TAB_ATELIER_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
        let bind = std::env::var("TAB_ATELIER_KIOSK_ADDR").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        Self::new(&upstream, &bind)
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

/// Deadlines for the upstream hop, one per stage that must produce something.
///
/// Split out from [`Upstream`] so a test can compress them and still exercise
/// the real timeout paths instead of waiting out the production values.
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

/// The daemon base URL plus the agent used to reach it.
///
/// One agent for the whole process: it owns the connection pool, and it is the
/// single place the hop's timeouts are configured.
#[derive(Debug)]
pub struct Upstream {
    pub base: String,
    pub agent: ureq::Agent,
}

impl Upstream {
    pub fn new(base: String, timeouts: HopTimeouts) -> Self {
        let agent = ureq::Agent::config_builder()
            // A 404 from the daemon is a 404 for the browser, not an upstream
            // failure to be rewritten into a 502.
            .http_status_as_error(false)
            // The browser follows redirects, and its cookie/authorization
            // handling must see the real target.
            .max_redirects(0)
            .timeout_resolve(timeouts.resolve)
            .timeout_connect(timeouts.connect)
            .timeout_send_request(timeouts.send_request)
            .timeout_send_body(timeouts.send_body)
            .timeout_recv_response(timeouts.recv_response)
            .timeout_recv_body(timeouts.recv_body)
            .build()
            .new_agent();
        Self { base, agent }
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
