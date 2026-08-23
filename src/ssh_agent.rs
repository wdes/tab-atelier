// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab ssh-agent lifecycle for the headless daemon.
//!
//! Each tab may own a dedicated `ssh-agent` (see [`crate::SshAgentConfig`]) so
//! different tabs can hold different SSH identities — or none — without sharing
//! the daemon's ambient agent. [`ensure`] spawns (or reuses) the agent and
//! returns the `SSH_AUTH_SOCK` path the spawn code injects into the tab's PTY;
//! [`teardown`] reaps it when the tab is closed.
//!
//! ## Lifetime vs. PTY respawn
//!
//! The agent is a **daemon child**, not a member of the tab's cgroup, so a PTY
//! respawn (net toggle, auto-resume) does not `cgroup.kill` it — [`ensure`] is
//! idempotent and hands back the same socket, so keys loaded earlier survive
//! the respawn. On `systemctl restart` the whole service cgroup dies (agents
//! included) and the persisted [`crate::TabState::ssh_agent`] config
//! re-provisions fresh agents on boot, so no orphans accumulate.
//!
//! ## Degradation
//!
//! Best-effort: a missing `ssh-agent` binary, an unwritable socket dir, or an
//! `ssh-add` failure never kills a tab — the tab just spawns without a per-tab
//! `SSH_AUTH_SOCK` (a one-line warning is logged).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// A live per-tab agent: its pid (to kill on teardown) and socket path.
struct Handle {
    pid: u32,
    sock: PathBuf,
}

/// Process-global registry keyed by tab id, mirroring [`crate::net_nft`]'s
/// per-tab table map. One daemon owns every tab's agent.
static AGENTS: LazyLock<Mutex<HashMap<String, Handle>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Directory holding the per-tab agent sockets. Prefers `$XDG_RUNTIME_DIR`
/// (a user session's private 0700 tmpfs) and otherwise falls back to the
/// service's state dir; a `ssh/` subdir keeps the sockets grouped.
fn sock_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(crate::platform::state_base_dir, PathBuf::from);
    base.join(crate::APP_DIR).join("ssh")
}

/// Socket path for a tab. The filename is `crc32(tab_id)` in hex so the full
/// path stays well under the `AF_UNIX` 108-byte limit regardless of how long the
/// base dir is; tab ids are UUIDs, so crc32 collisions are not a concern.
fn sock_path(tab_id: &str) -> PathBuf {
    sock_dir().join(format!("{:08x}.sock", crate::crc32(tab_id.as_bytes())))
}

/// Restrict a freshly created dir to owner-only (0700) on unix. No-op
/// elsewhere — `$XDG_RUNTIME_DIR` / the state dir already carry safe perms.
#[cfg(unix)]
fn lock_down(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn lock_down(_dir: &std::path::Path) {}

/// Ensure a live ssh-agent for `tab_id`, returning its socket path.
///
/// Idempotent: a respawn reuses the existing agent (so loaded keys persist).
/// `key`, when `Some`, is a passphrase-less private key `ssh-add`ed in a
/// detached thread so the latency-sensitive spawn path never blocks. Returns
/// `None` (logged) if `ssh-agent` can't be started.
#[must_use]
pub fn ensure(tab_id: &str, key: Option<&str>) -> Option<PathBuf> {
    let mut map = AGENTS.lock().ok()?;
    if let Some(h) = map.get(tab_id) {
        return Some(h.sock.clone());
    }

    let dir = sock_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("ssh_agent: cannot create socket dir {}: {e}", dir.display());
        return None;
    }
    lock_down(&dir);

    let sock = sock_path(tab_id);
    // A stale socket from a crashed/killed prior agent would make `ssh-agent
    // -a` refuse to bind; unlink it first (best-effort).
    let _ = std::fs::remove_file(&sock);

    let out = match std::process::Command::new("ssh-agent").arg("-a").arg(&sock).output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            log::warn!(
                "ssh_agent: ssh-agent exited {} for tab {tab_id}; no per-tab agent",
                o.status
            );
            return None;
        }
        Err(e) => {
            log::warn!("ssh_agent: could not spawn ssh-agent for tab {tab_id}: {e}");
            return None;
        }
    };

    // `ssh-agent -a` prints `SSH_AGENT_PID=<pid>; export SSH_AGENT_PID;` — we
    // parse the pid so teardown can kill exactly this agent.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pid = parse_agent_pid(&stdout).unwrap_or(0);

    map.insert(
        tab_id.to_string(),
        Handle {
            pid,
            sock: sock.clone(),
        },
    );
    drop(map);

    if let Some(key) = key {
        add_key(sock.clone(), key.to_string(), tab_id.to_string());
    }
    Some(sock)
}

/// `ssh-add <key>` against the tab's agent, off the spawn path.
///
/// Runs in a detached thread (spawn latency matters) with `stdin` closed so an
/// *encrypted* key fails fast at the passphrase prompt instead of hanging with
/// no tty. Best-effort: a failure only means the key isn't loaded; the user can
/// `ssh-add` it themselves inside the tab.
fn add_key(sock: PathBuf, key: String, tab_id: String) {
    std::thread::spawn(move || {
        let status = std::process::Command::new("ssh-add")
            .arg(&key)
            .env("SSH_AUTH_SOCK", &sock)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => log::debug!("ssh_agent: loaded key {key} into tab {tab_id}"),
            Ok(s) => {
                log::warn!("ssh_agent: ssh-add {key} for tab {tab_id} exited {s} (encrypted key? load it in the tab)");
            }
            Err(e) => log::warn!("ssh_agent: could not run ssh-add for tab {tab_id}: {e}"),
        }
    });
}

/// Pull the pid out of `ssh-agent`'s output. Handles both the Bourne form
/// (`SSH_AGENT_PID=<pid>;`, what `ssh-agent -a` emits) and the csh form
/// (`setenv SSH_AGENT_PID <pid>;`). Pure so it's unit-testable.
#[must_use]
fn parse_agent_pid(stdout: &str) -> Option<u32> {
    let tail = stdout.split("SSH_AGENT_PID").nth(1)?;
    tail.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Kill a tab's agent and unlink its socket. Best-effort and idempotent — a
/// tab that never had an agent is a no-op. Called on tab close, NOT on respawn.
pub fn teardown(tab_id: &str) {
    let Some(h) = AGENTS.lock().ok().and_then(|mut m| m.remove(tab_id)) else {
        return;
    };
    if h.pid != 0 {
        // `ssh-agent -k` needs the env to find itself; a direct kill is simpler
        // and we hold the pid.
        let _ = std::process::Command::new("kill")
            .arg(h.pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(&h.sock);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_pid_from_bourne_output() {
        let out = "SSH_AUTH_SOCK=/tmp/x.sock; export SSH_AUTH_SOCK;\nSSH_AGENT_PID=12345; export SSH_AGENT_PID;\necho Agent pid 12345;\n";
        assert_eq!(parse_agent_pid(out), Some(12345));
    }

    #[test]
    fn parses_agent_pid_from_csh_output() {
        let out = "setenv SSH_AUTH_SOCK /tmp/x.sock;\nsetenv SSH_AGENT_PID 999;\n";
        assert_eq!(parse_agent_pid(out), Some(999));
    }

    #[test]
    fn no_pid_when_absent() {
        assert_eq!(parse_agent_pid("nothing here"), None);
    }

    #[test]
    fn sock_path_is_short_and_stable() {
        let a = sock_path("16eb00d6-17e7-48c2-9f3a-000000000000");
        let b = sock_path("16eb00d6-17e7-48c2-9f3a-000000000000");
        assert_eq!(a, b, "same id ⇒ same socket (idempotent respawn)");
        assert!(a.file_name().unwrap().to_string_lossy().ends_with(".sock"));
        // crc32 hex is 8 chars + ".sock" — comfortably under AF_UNIX's limit.
        assert!(a.file_name().unwrap().len() <= 13);
    }
}
