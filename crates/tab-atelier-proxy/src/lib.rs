// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier-proxy` — a multi-user Anthropic API proxy for a tab-atelier
//! fleet.
//!
//! # What moved here, and why
//!
//! tab-atelier could already relay a Claude tab's API calls through another
//! tab-atelier, and that remote end authenticated everyone with ONE shared
//! token. A single secret cannot say who spent the quota, cannot be taken away
//! from one machine without re-keying every other, and keeps working for a
//! laptop that has left the building.
//!
//! So the far end is its own program now, with an account per person and a key
//! per account. It is also a better fit for where that end actually runs: a
//! server, headless, as its own user — not something you should have to
//! install a terminal emulator to obtain.
//!
//! The near end did not move. Both tab-atelier editions still speak
//! `relay on` / `relay via`; they just point at this instead.
//!
//! # The two credentials
//!
//! * a **user key** opens the proxied Anthropic path and nothing else. It goes
//!   in a developer's environment, so it travels; it must not be able to
//!   administer anything.
//! * the **admin token** opens the account API and the web UI. It stays put.
//!
//! Neither is the Claude credential: that is the proxy host's own OAuth login,
//! it never leaves the machine, and a leaked user key cannot be replayed
//! against Anthropic directly.

pub mod account;
pub mod egress;
pub mod provider;
pub mod qos;
pub mod routing;
pub mod server;
pub mod usage;
pub mod users;

use std::path::{Path, PathBuf};

/// Where the SECRETS live: accounts (with their key hashes) and the admin
/// token.
///
/// Separate from [`state_dir`] on purpose. Identity material belongs under
/// `~/.config`, which is what people back up and what survives a "clear the
/// caches" sweep; `~/.local/state` is for things a program can regenerate —
/// here, the usage history. Losing state costs you a graph. Losing this costs
/// everyone their access.
///
/// `$TAB_ATELIER_PROXY_CONFIG` wins, then `$XDG_CONFIG_HOME`, then
/// `~/.config`. Under the shipped systemd unit both land in
/// `/var/lib/tab-atelier-proxy`: a system service has no home directory, and
/// `StateDirectory=` is the one place systemd guarantees is writable and
/// 0700.
///
/// # Errors
/// Neither the override nor `$HOME` is set, so there is nowhere to put it.
pub fn config_dir() -> Result<PathBuf, String> {
    resolve_dir(
        std::env::var_os("TAB_ATELIER_PROXY_CONFIG").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        ".config",
    )
}

/// Where the usage history lives.
///
/// `$TAB_ATELIER_PROXY_STATE` wins, then `$XDG_STATE_HOME`, then
/// `~/.local/state`. Under the shipped systemd unit this lands in
/// `/var/lib/tab-atelier-proxy` via `StateDirectory=`.
///
/// # Errors
/// Neither the override nor `$HOME` is set, so there is nowhere to put it.
pub fn state_dir() -> Result<PathBuf, String> {
    resolve_dir(
        std::env::var_os("TAB_ATELIER_PROXY_STATE").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        ".local/state",
    )
}

/// The precedence, separated from reading the environment so it can be tested.
///
/// Mutating the process environment in a test is both `unsafe` (forbidden in
/// this crate) and racy with every other test in the binary.
fn resolve_dir(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
    home_relative: &str,
) -> Result<PathBuf, String> {
    resolve_dir_within(explicit, xdg, home, home_relative, Path::new(SYSTEM_DIR))
}

/// Where a packaged install keeps everything.
///
/// The systemd unit points both directories here explicitly, and a service
/// account has no home to split them across.
pub const SYSTEM_DIR: &str = "/var/lib/tab-atelier-proxy";

/// [`resolve_dir`] with the system location supplied, so it can be tested
/// without a `/var/lib` to look at.
fn resolve_dir_within(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
    home_relative: &str,
    system: &Path,
) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let user = xdg
        .map(|p| p.join("tab-atelier-proxy"))
        .or_else(|| home.map(|h| h.join(home_relative).join("tab-atelier-proxy")));

    // A user directory that already holds something wins: somebody running
    // their own proxy on a machine that also has the packaged service must not
    // be quietly redirected into the service's data.
    if user.as_deref().is_some_and(has_data) {
        return user.ok_or_else(|| unreachable!());
    }
    // Otherwise, if a packaged install is present, that is the deployment
    // being asked about. This is what makes
    // `sudo -u tab-atelier-proxy tab-atelier-proxy admin-token` print the
    // token the SERVICE uses: the unit's Environment= does not reach a command
    // run by hand, so without this the CLI reads a different directory and
    // mints a second token that authenticates nothing.
    if has_data(system) {
        return Ok(system.to_path_buf());
    }
    user.ok_or_else(|| "no $HOME and no TAB_ATELIER_PROXY_{CONFIG,STATE}".to_owned())
}

/// Whether a directory exists and holds anything at all.
fn has_data(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some())
}

/// Read the admin token, minting one on first run.
///
/// Lives beside the accounts, under [`config_dir`], so one `chmod 700` covers
/// every secret the proxy owns.
///
/// # Errors
/// The state directory is unusable.
pub fn admin_token(dir: &std::path::Path) -> Result<String, String> {
    let path = dir.join("admin.token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_owned();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let minted = users::mint_key();
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::write(&path, &minted).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(minted)
}

/// Where the web UI's files are.
///
/// The packaged location first, then the source tree, so `cargo run` in a
/// checkout serves the UI without an install step.
#[must_use]
pub fn web_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TAB_ATELIER_PROXY_WEB") {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    [
        PathBuf::from("/usr/share/tab-atelier-proxy/web"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"),
    ]
    .into_iter()
    .find(|c| c.join("index.html").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A packaged install is found when the caller has no data of their own,
    /// which is what makes the documented `sudo -u …` command work: the unit's
    /// Environment= does not reach a command run by hand.
    #[test]
    fn a_packaged_install_is_preferred_over_an_empty_home() {
        let tmp = std::env::temp_dir().join(format!("ta-proxy-res-{}", uuid::Uuid::new_v4()));
        let system = tmp.join("var-lib");
        let home = tmp.join("home");
        std::fs::create_dir_all(&system).expect("mkdir");
        std::fs::create_dir_all(&home).expect("mkdir");
        std::fs::write(system.join("admin.token"), "tap_x").expect("write");

        // Nothing in the user's home → the packaged install is the answer.
        assert_eq!(
            resolve_dir_within(None, None, Some(home.clone()), ".config", &system).expect("dir"),
            system
        );

        // But a user who HAS their own data keeps it: running your own proxy
        // on a machine that also runs the service must not be hijacked.
        let mine = home.join(".config").join("tab-atelier-proxy");
        std::fs::create_dir_all(&mine).expect("mkdir");
        std::fs::write(mine.join("users.json"), "{}").expect("write");
        assert_eq!(
            resolve_dir_within(None, None, Some(home.clone()), ".config", &system).expect("dir"),
            mine
        );

        // And an explicit override beats both, always.
        assert_eq!(
            resolve_dir_within(Some(PathBuf::from("/explicit")), None, Some(home), ".config", &system).expect("dir"),
            PathBuf::from("/explicit")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_state_dir_follows_its_precedence() {
        let p = |s: &str| PathBuf::from(s);
        let no_system = p("/nonexistent-system-dir-for-this-test");
        // The explicit override wins, which is what the systemd unit relies on
        // to keep state in /var/lib rather than the service account's home.
        assert_eq!(
            resolve_dir(
                Some(p("/explicit")),
                Some(p("/xdg")),
                Some(p("/home/u")),
                ".local/state"
            )
            .expect("dir"),
            p("/explicit")
        );
        assert_eq!(
            resolve_dir_within(None, Some(p("/xdg")), Some(p("/home/u")), ".local/state", &no_system).expect("dir"),
            p("/xdg/tab-atelier-proxy")
        );
        assert_eq!(
            resolve_dir_within(None, None, Some(p("/home/u")), ".local/state", &no_system).expect("dir"),
            p("/home/u/.local/state/tab-atelier-proxy")
        );
        // Nowhere to put accounts is an error, not a guess at /tmp.
        assert!(resolve_dir_within(None, None, None, ".local/state", &no_system).is_err());
        // Secrets and history are deliberately different directories.
        assert_eq!(
            resolve_dir_within(None, None, Some(p("/home/u")), ".config", &no_system).expect("dir"),
            p("/home/u/.config/tab-atelier-proxy")
        );
    }

    /// Minting on absence is the right behaviour and also a trap: if the file
    /// moves and nothing carries it across, the next start issues a NEW token
    /// and the one the operator wrote down begins answering "admin token
    /// required" — with nothing to say it was replaced.
    #[test]
    fn a_token_is_never_silently_replaced_by_a_new_one() {
        let dir = std::env::temp_dir().join(format!("ta-proxy-tok-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("admin.token"),
            "tap_existing
",
        )
        .expect("write");
        assert_eq!(
            admin_token(&dir).expect("read"),
            "tap_existing",
            "an existing token must be returned as-is (trimmed), never re-minted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_admin_token_is_minted_once_and_then_reused() {
        let dir = std::env::temp_dir().join(format!("ta-proxy-admin-{}", uuid::Uuid::new_v4()));
        let first = admin_token(&dir).expect("mint");
        let second = admin_token(&dir).expect("reuse");
        assert_eq!(first, second, "a restart must not invalidate the operator's token");
        assert!(first.starts_with(users::KEY_PREFIX));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
