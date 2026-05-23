// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Delegate Wakatime heartbeats to the canonical `wakatime-cli` binary
//! instead of talking HTTP ourselves. This mirrors the architecture
//! every official Wakatime editor extension uses — see
//! <https://github.com/wakatime/zed-wakatime> for a reference — so
//! auth handling, offline queueing, project / language detection and
//! the User-Agent header are all maintained by Wakatime upstream.
//!
//! We do not download the CLI; the user is expected to already have it
//! (Zed-wakatime, the official wakatime-cli installer, or
//! `~/.wakatime/wakatime-cli-*` left over from another editor all
//! work). If the binary isn't found, tracking is silently disabled.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use log::{debug, warn};

/// Mirror wakatime-cli's own `--heartbeat-rate-limit-seconds` default.
/// Anything more frequent would be batched away by the CLI anyway, so
/// skipping the subprocess spawn is the only saving here.
const DEBOUNCE_SECS: u64 = 120;

/// Generic identifier returned by the `/tabs` API and shown in the
/// mobile remote's "app" field. Unrelated to the Wakatime UA — that
/// one is now owned entirely by wakatime-cli.
pub const USER_AGENT: &str = concat!(
    "tab-atelier/",
    env!("CARGO_PKG_VERSION"),
    " (terminal; +https://github.com/wdes/tab-atelier)"
);

/// Sentinel passed to `wakatime-cli --plugin` so the dashboard groups
/// heartbeats under a "tab-atelier" editor row instead of the catch-all
/// "Other" bucket. Format follows wakatime-cli's documented convention:
/// `EditorName/version`.
const PLUGIN_NAME: &str = concat!("tab-atelier/", env!("CARGO_PKG_VERSION"));

pub enum HeartbeatEvent {
    Activity { cwd: Option<PathBuf> },
    Shutdown,
}

pub struct WakatimeTracker {
    tx: mpsc::Sender<HeartbeatEvent>,
}

impl WakatimeTracker {
    /// Create a tracker if a usable `wakatime-cli` is on disk. Returns
    /// `None` (and logs) otherwise so the rest of the app can run
    /// without time tracking. The api key is optional — when omitted,
    /// wakatime-cli reads `api_key` from `~/.wakatime.cfg` itself.
    pub fn new(api_key: Option<String>) -> Option<Self> {
        let cli = locate_wakatime_cli()?;
        debug!("wakatime: using cli at {}", cli.display());
        let (tx, rx) = mpsc::channel::<HeartbeatEvent>();

        std::thread::spawn(move || {
            let mut last_sent: u64 = 0;
            let mut last_project: Option<String> = None;

            while let Ok(HeartbeatEvent::Activity { cwd }) = rx.recv() {
                let now = unix_now();
                let project = cwd.as_deref().and_then(detect_project);
                let project_changed = project != last_project;

                if now - last_sent < DEBOUNCE_SECS && !project_changed {
                    continue;
                }
                last_sent = now;
                last_project.clone_from(&project);

                debug!("wakatime: heartbeat project={project:?}");
                if let Err(e) = send_heartbeat(&cli, api_key.as_deref(), cwd.as_deref(), project.as_deref()) {
                    warn!("{e}");
                }
            }
        });

        Some(Self { tx })
    }

    pub fn record_activity(&self, cwd: Option<PathBuf>) {
        let _ = self.tx.send(HeartbeatEvent::Activity { cwd });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(HeartbeatEvent::Shutdown);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn detect_project(cwd: &Path) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return dir.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        if !dir.pop() {
            break;
        }
    }
    cwd.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Find `wakatime-cli`, preferring whatever's on `$PATH` (the official
/// installer drops a symlink there) and falling back to the per-user
/// install Zed-wakatime, Claude-wakatime, etc. land at
/// `~/.wakatime/wakatime-cli-<os>-<arch>`.
fn locate_wakatime_cli() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("wakatime-cli");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var("HOME").ok()?;
    let waka_dir = PathBuf::from(home).join(".wakatime");
    let entries = std::fs::read_dir(&waka_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // wakatime-cli ships as `wakatime-cli-linux-amd64`,
        // `wakatime-cli-darwin-arm64`, `wakatime-cli-windows-amd64.exe`,
        // etc. Match the prefix and pick the first hit — there's only
        // one valid binary per host.
        if name.starts_with("wakatime-cli-") && !name.ends_with(".sha256") {
            let p = entry.path();
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn send_heartbeat(cli: &Path, api_key: Option<&str>, cwd: Option<&Path>, project: Option<&str>) -> Result<(), String> {
    let mut cmd = std::process::Command::new(cli);
    cmd.arg("--entity")
        .arg("tab-atelier")
        .arg("--entity-type")
        .arg("app")
        .arg("--plugin")
        .arg(PLUGIN_NAME)
        .arg("--category")
        .arg("coding");

    if let Some(p) = project {
        cmd.arg("--alternate-project").arg(p);
    }
    if let Some(d) = cwd {
        cmd.arg("--project-folder").arg(d);
    }
    if let Some(k) = api_key {
        cmd.arg("--key").arg(k);
    }

    let out = cmd
        .output()
        .map_err(|e| format!("wakatime: spawn {}: {e}", cli.display()))?;
    if out.status.success() {
        debug!("wakatime: heartbeat sent");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("wakatime: cli exit {:?}: {}", out.status.code(), stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_project_finds_git() {
        let dir = std::env::temp_dir().join("ta-test-detect-project");
        let sub = dir.join("myproject");
        let _ = std::fs::create_dir_all(sub.join(".git"));
        let deep = sub.join("src/nested");
        let _ = std::fs::create_dir_all(&deep);

        assert_eq!(detect_project(&deep), Some("myproject".into()));
        assert_eq!(detect_project(&sub), Some("myproject".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_project_no_git_uses_dirname() {
        let dir = std::env::temp_dir().join("ta-test-no-git");
        let leaf = dir.join("somefolder");
        let _ = std::fs::create_dir_all(&leaf);

        assert_eq!(detect_project(&leaf), Some("somefolder".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_agent_contains_version() {
        assert!(USER_AGENT.starts_with("tab-atelier/"));
        assert!(USER_AGENT.contains("terminal"));
        assert!(USER_AGENT.contains("github.com/wdes/tab-atelier"));
    }

    #[test]
    fn plugin_name_has_editor_version_shape() {
        // wakatime-cli's --plugin parser splits on `/`; the dashboard
        // shows the left side as editor name, right side as version.
        let (editor, ver) = PLUGIN_NAME.split_once('/').expect("plugin name has /");
        assert_eq!(editor, "tab-atelier");
        assert!(!ver.is_empty());
    }

    #[test]
    fn unix_now_is_reasonable() {
        let now = unix_now();
        assert!(now > 1_700_000_000);
    }

    #[test]
    fn locate_cli_or_none() {
        // Whatever the host has — just verify the function doesn't panic
        // and returns either a valid file path or None.
        if let Some(path) = locate_wakatime_cli() {
            assert!(path.is_file(), "{} should exist", path.display());
        }
    }

    #[test]
    fn detect_project_walks_until_root() {
        assert_eq!(detect_project(Path::new("/")), None);
    }

    #[test]
    fn detect_project_picks_nearest_git_ancestor() {
        let root = std::env::temp_dir().join("ta-test-nested-git");
        let outer = root.join("outer");
        let inner = outer.join("inner");
        let leaf = inner.join("src");
        let _ = std::fs::create_dir_all(&leaf);
        let _ = std::fs::create_dir_all(outer.join(".git"));
        let _ = std::fs::create_dir_all(inner.join(".git"));

        assert_eq!(detect_project(&leaf), Some("inner".into()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_agent_version_matches_cargo_pkg() {
        let expected = format!("tab-atelier/{}", env!("CARGO_PKG_VERSION"));
        assert!(USER_AGENT.starts_with(&expected), "got {USER_AGENT}");
        assert!(USER_AGENT.ends_with(')'));
    }

    #[test]
    fn plugin_name_version_matches_cargo_pkg() {
        let (_, ver) = PLUGIN_NAME.split_once('/').unwrap();
        assert_eq!(ver, env!("CARGO_PKG_VERSION"));
        assert_eq!(PLUGIN_NAME.matches('/').count(), 1);
    }

    #[test]
    fn debounce_predicate_matches_inline_logic() {
        let should_skip = |now: u64, last_sent: u64, project_changed: bool| {
            now - last_sent < DEBOUNCE_SECS && !project_changed
        };
        assert_eq!(DEBOUNCE_SECS, 120);
        assert!(should_skip(1_000, 999, false));
        assert!(!should_skip(1_000, 999, true));
        assert!(!should_skip(2_000, 1_000, false));
        assert!(!should_skip(120, 0, false));
    }

    #[test]
    fn send_heartbeat_missing_binary_errors_cleanly() {
        let fake = std::env::temp_dir().join("ta-test-nonexistent-wakatime-cli-xyz");
        let _ = std::fs::remove_file(&fake);
        let err = send_heartbeat(&fake, Some("k"), Some(Path::new("/tmp")), Some("proj"))
            .expect_err("missing binary must error");
        assert!(err.starts_with("wakatime: spawn "), "got {err}");
        assert!(err.contains(&fake.display().to_string()));
    }

    #[test]
    fn heartbeat_event_variants_send_through_channel() {
        let (tx, rx) = mpsc::channel::<HeartbeatEvent>();
        tx.send(HeartbeatEvent::Activity { cwd: None }).unwrap();
        tx.send(HeartbeatEvent::Activity { cwd: Some(PathBuf::from("/x")) }).unwrap();
        tx.send(HeartbeatEvent::Shutdown).unwrap();

        assert!(matches!(rx.recv().unwrap(), HeartbeatEvent::Activity { cwd: None }));
        assert!(matches!(rx.recv().unwrap(), HeartbeatEvent::Activity { cwd: Some(_) }));
        assert!(matches!(rx.recv().unwrap(), HeartbeatEvent::Shutdown));
    }

    #[test]
    fn tracker_send_methods_are_silent_after_worker_exit() {
        let (tx, rx) = mpsc::channel::<HeartbeatEvent>();
        drop(rx);
        let tracker = WakatimeTracker { tx };
        tracker.record_activity(Some(PathBuf::from("/tmp")));
        tracker.record_activity(None);
        tracker.shutdown();
    }

}
