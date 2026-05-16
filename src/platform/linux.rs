// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use log::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

use crate::platform::CapturedImage;

// --- Directories ---

pub fn state_base_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        },
        PathBuf::from,
    )
}

pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        },
        PathBuf::from,
    )
}

pub fn pictures_dir() -> PathBuf {
    let pictures = std::process::Command::new("xdg-user-dir")
        .arg("PICTURES")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/Pictures")
        });
    PathBuf::from(pictures)
}

// --- Process ---

pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

pub fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// --- Random ---

pub fn random_bytes(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in seed.to_le_bytes().iter().cycle().take(buf.len()).enumerate() {
            buf[i] = *b;
        }
    }
}

// --- Screenshot (X11) ---

pub fn capture_focused_window() -> Result<CapturedImage, String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("x11 connect: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let focus = conn
        .get_input_focus()
        .map_err(|e| format!("get_input_focus: {e}"))?
        .reply()
        .map_err(|e| format!("get_input_focus reply: {e}"))?;

    let mut window = focus.focus;
    loop {
        let tree = conn
            .query_tree(window)
            .map_err(|e| format!("query_tree: {e}"))?
            .reply()
            .map_err(|e| format!("query_tree reply: {e}"))?;
        if tree.parent == root || tree.parent == 0 {
            break;
        }
        window = tree.parent;
    }

    let geom = conn
        .get_geometry(window)
        .map_err(|e| format!("get_geometry: {e}"))?
        .reply()
        .map_err(|e| format!("get_geometry reply: {e}"))?;

    let coords = conn
        .translate_coordinates(window, root, 0, 0)
        .map_err(|e| format!("translate_coordinates: {e}"))?
        .reply()
        .map_err(|e| format!("translate_coordinates reply: {e}"))?;

    debug!(
        "screenshot: capturing from root at ({},{}) size {}x{}",
        coords.dst_x, coords.dst_y, geom.width, geom.height
    );

    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            coords.dst_x,
            coords.dst_y,
            geom.width,
            geom.height,
            u32::MAX,
        )
        .map_err(|e| format!("get_image: {e}"))?
        .reply()
        .map_err(|e| format!("get_image reply: {e}"))?;

    Ok(CapturedImage {
        width: geom.width,
        height: geom.height,
        data: reply.data,
    })
}

// --- Openers ---

pub fn open_url(url: &str, browser: Option<&str>) {
    let cmd = browser.unwrap_or("xdg-open");
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn open_path(path: &std::path::Path, editor: Option<&str>) {
    let cmd = editor.unwrap_or("xdg-open");
    let _ = std::process::Command::new(cmd)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// --- Hotkeys (X11) ---

enum HotkeyCommand {
    UpdateKeys(Vec<u8>),
    Suspend,
    Resume,
}

pub fn grab_hotkeys<F>(keycodes: &[u8], on_press: F) -> HotkeyHandle
where
    F: Fn() + Send + 'static,
{
    use std::sync::mpsc;
    use x11rb::protocol::xproto::{GrabMode, ModMask};

    let (cmd_tx, cmd_rx) = mpsc::channel::<HotkeyCommand>();

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return HotkeyHandle { cmd_tx };
    };

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let masks = [
        ModMask::default(),
        ModMask::LOCK,
        ModMask::from(u16::from(ModMask::M2)),
        ModMask::LOCK | ModMask::from(u16::from(ModMask::M2)),
    ];

    for &keycode in keycodes {
        for mask in masks {
            let _ = conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC);
        }
    }
    let _ = conn.flush();

    let initial_keys = keycodes.to_vec();

    std::thread::spawn(move || {
        let mut active_keys = initial_keys;
        let mut suspended = false;

        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    HotkeyCommand::UpdateKeys(new_keys) => {
                        for &keycode in &active_keys {
                            for mask in masks {
                                let _ = conn.ungrab_key(keycode, root, mask);
                            }
                        }
                        for &keycode in &new_keys {
                            for mask in masks {
                                let _ = conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC);
                            }
                        }
                        let _ = conn.flush();
                        active_keys = new_keys;
                        suspended = false;
                    }
                    HotkeyCommand::Suspend => {
                        for &keycode in &active_keys {
                            for mask in masks {
                                let _ = conn.ungrab_key(keycode, root, mask);
                            }
                        }
                        let _ = conn.flush();
                        suspended = true;
                    }
                    HotkeyCommand::Resume => {
                        for &keycode in &active_keys {
                            for mask in masks {
                                let _ = conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC);
                            }
                        }
                        let _ = conn.flush();
                        suspended = false;
                    }
                }
            }

            match conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let x11rb::protocol::Event::KeyPress(kp) = event
                        && !suspended
                        && active_keys.contains(&kp.detail)
                    {
                        on_press();
                    }
                }
                Ok(None) => {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                Err(_) => break,
            }
        }
    });

    HotkeyHandle { cmd_tx }
}

pub struct HotkeyHandle {
    cmd_tx: std::sync::mpsc::Sender<HotkeyCommand>,
}

impl HotkeyHandle {
    pub fn update_keys(&self, new_keycodes: &[u8]) {
        let _ = self.cmd_tx.send(HotkeyCommand::UpdateKeys(new_keycodes.to_vec()));
    }

    pub fn suspend(&self) {
        let _ = self.cmd_tx.send(HotkeyCommand::Suspend);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(HotkeyCommand::Resume);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_base_dir_not_empty() {
        let dir = state_base_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn config_dir_not_empty() {
        let dir = config_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn pictures_dir_not_empty() {
        let dir = pictures_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn random_bytes_fills_buffer() {
        let mut buf = [0u8; 16];
        random_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn process_cwd_of_self() {
        let pid = std::process::id();
        let cwd = process_cwd(pid);
        assert!(cwd.is_some());
    }

    #[test]
    fn process_alive_self() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_alive_bogus() {
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn process_cwd_bogus_returns_none() {
        assert_eq!(process_cwd(u32::MAX), None);
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
    fn open_url_does_not_panic() {
        open_url("http://localhost:1", None);
        open_url("http://localhost:1", Some("false"));
    }

    #[test]
    fn open_path_does_not_panic() {
        open_path(std::path::Path::new("/dev/null"), None);
        open_path(std::path::Path::new("/dev/null"), Some("false"));
    }

    #[test]
    fn hotkey_handle_methods_without_display() {
        use std::sync::mpsc;
        let (cmd_tx, _rx) = mpsc::channel::<HotkeyCommand>();
        let handle = HotkeyHandle { cmd_tx };
        handle.update_keys(&[49, 96]);
        handle.suspend();
        handle.resume();
    }
}
