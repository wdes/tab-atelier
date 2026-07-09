// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Windows platform layer — mirrors the public surface of `linux.rs`.
//!
//! Directory mapping (Windows has no XDG split, so we follow the
//! `%APPDATA%` / `%LOCALAPPDATA%` convention instead):
//!
//! | linux.rs                         | windows.rs                         |
//! |----------------------------------|------------------------------------|
//! | `state_base_dir` `$XDG_STATE_HOME` / `~/.local/state` | `%LOCALAPPDATA%` (non-roaming: logs, tab output) |
//! | `config_base_dir` `~/.local`     | `%APPDATA%` (roaming: tabs.json, preferences.json live under `<base>\tab-atelier`) |
//! | `config_dir` `~/.config`         | `%APPDATA%\tab-atelier` (roaming: signing key, relay secret) |
//!
//! Callers append `crate::APP_DIR` to `state_base_dir` / `config_base_dir`
//! exactly as on Linux, so state ends up at `%LOCALAPPDATA%\tab-atelier`
//! and tabs/preferences at `%APPDATA%\tab-atelier`.
//!
//! Stubs awaiting the rest of the Windows port (tracked as follow-ups,
//! all GUI-only so the headless build is unaffected):
//!   * `capture_focused_window` — needs GDI `BitBlt` / Windows.Graphics.Capture.
//!   * `grab_hotkeys` — needs `RegisterHotKey` + a message-pump thread.
//! And `process_cwd` (non-GUI) needs `NtQueryInformationProcess` to read
//! another process's PEB; returns `None` until then.

#[cfg(feature = "gui")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(feature = "gui")]
use crate::platform::CapturedImage;

// --- Directories ---

/// `%USERPROFILE%` (e.g. `C:\Users\alice`), falling back to the system
/// temp dir if the env var is somehow unset (service accounts, stripped
/// environments).
fn user_profile() -> PathBuf {
    std::env::var("USERPROFILE").map_or_else(|_| std::env::temp_dir(), PathBuf::from)
}

/// `%LOCALAPPDATA%` (e.g. `C:\Users\alice\AppData\Local`).
fn local_app_data() -> PathBuf {
    std::env::var("LOCALAPPDATA").map_or_else(|_| user_profile().join("AppData").join("Local"), PathBuf::from)
}

/// `%APPDATA%` — the roaming profile (e.g. `C:\Users\alice\AppData\Roaming`).
fn roaming_app_data() -> PathBuf {
    std::env::var("APPDATA").map_or_else(|_| user_profile().join("AppData").join("Roaming"), PathBuf::from)
}

pub fn state_base_dir() -> PathBuf {
    local_app_data()
}

/// Base for tab metadata (tabs.json) and preferences.json. Callers join
/// `crate::APP_DIR`, so these land under `%APPDATA%\tab-atelier`.
pub fn config_base_dir() -> PathBuf {
    roaming_app_data()
}

/// Where persistent identity material (auth tokens, TLS cert + key)
/// is written. On Linux this is the bare `~/.config`; on Windows we
/// namespace under the app folder so we don't litter `%APPDATA%` root.
pub fn config_dir() -> PathBuf {
    roaming_app_data().join(crate::APP_DIR)
}

#[cfg(feature = "gui")]
pub fn pictures_dir() -> PathBuf {
    // The Known Folder API (SHGetKnownFolderPath/FOLDERID_Pictures) is
    // the strictly-correct source and honours a relocated Pictures
    // library; %USERPROFILE%\Pictures is the default location and is
    // good enough until the GUI port wires up the COM call.
    user_profile().join("Pictures")
}

// --- Process ---

pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    // No cheap equivalent of /proc/<pid>/cwd. Reading another process's
    // current directory means NtQueryInformationProcess + walking the
    // PEB (ProcessParameters->CurrentDirectory). Deferred; callers treat
    // `None` as "unknown cwd".
    None
}

// --- Random ---

pub fn random_bytes(buf: &mut [u8]) {
    // getrandom wraps BCryptGenRandom (system-preferred RNG) on Windows.
    // 0.4 API: `fill(buf) -> Result<(), Error>`; the older `getrandom`
    // free-fn was removed at 0.3.
    if getrandom::fill(buf).is_err() {
        // Mirror linux.rs's degraded fallback so we never hard-fail.
        // Essentially unreachable on a healthy system; NOT crypto-grade.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in seed.to_le_bytes().iter().cycle().take(buf.len()).enumerate() {
            buf[i] = *b;
        }
    }
}

// --- Screenshot ---

#[cfg(feature = "gui")]
pub fn capture_focused_window() -> Result<CapturedImage, String> {
    // TODO(windows-port): GetForegroundWindow + GetWindowRect, then a
    // GDI BitBlt into a DIB section (or Windows.Graphics.Capture for
    // composited/DWM-correct output). Returns an error for now so the
    // screenshot feature degrades gracefully instead of failing to build.
    Err("screenshot capture is not yet implemented on Windows".to_string())
}

// --- Openers ---

#[cfg(feature = "gui")]
pub fn open_url(url: &str, browser: Option<&str>) {
    match browser {
        // Explicit handler: launch it directly with the URL as argv[1].
        Some(cmd) => {
            let _ = std::process::Command::new(cmd)
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        // Default handler via the shell `start` builtin. The empty ""
        // is `start`'s title argument — required so a quoted URL isn't
        // misread as the window title.
        None => {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

#[cfg(feature = "gui")]
pub fn open_path(path: &Path, editor: Option<&str>) {
    match editor {
        Some(cmd) => {
            let _ = std::process::Command::new(cmd)
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        None => {
            let _ = std::process::Command::new("cmd")
                .arg("/C")
                .arg("start")
                .arg("")
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

// --- Hotkeys ---

#[cfg(feature = "gui")]
pub fn grab_hotkeys<F>(_keycodes: &[u8], _on_press: F) -> HotkeyHandle
where
    F: Fn() + Send + 'static,
{
    // TODO(windows-port): register a hidden message-only window on a
    // dedicated thread, RegisterHotKey() the keycodes, and dispatch
    // WM_HOTKEY to `on_press`. The Linux impl uses X11 passive grabs;
    // there's no direct analogue, so this needs its own thread + pump.
    // Returns an inert handle so the global show/hide hotkey is simply
    // inactive on Windows for now.
    HotkeyHandle
}

#[cfg(feature = "gui")]
pub struct HotkeyHandle;

#[cfg(feature = "gui")]
impl HotkeyHandle {
    pub fn update_keys(&self, _new_keycodes: &[u8]) {}

    pub fn suspend(&self) {}

    pub fn resume(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_base_dir_not_empty() {
        assert!(!state_base_dir().as_os_str().is_empty());
    }

    #[test]
    fn config_dir_under_app_dir() {
        assert!(config_dir().ends_with(crate::APP_DIR));
    }

    #[test]
    fn random_bytes_fills_buffer() {
        let mut buf = [0u8; 16];
        random_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn random_bytes_different_calls() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        random_bytes(&mut a);
        random_bytes(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn process_cwd_is_none() {
        assert_eq!(process_cwd(std::process::id()), None);
    }
}
