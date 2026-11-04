// SPDX-License-Identifier: MPL-2.0

//! Desktop notifications through `notify-send` (libnotify).
//!
//! Shelling out keeps a D-Bus client out of the binary; a machine without
//! `notify-send` simply gets no notification.

/// Fire a notification without blocking the caller.
pub fn send(summary: &str, body: &str, critical: bool) {
    let child = std::process::Command::new("notify-send")
        .args(["--app-name", "Tab Atelier", "--icon", "tab-atelier", "--urgency"])
        .arg(if critical { "critical" } else { "normal" })
        .arg("--")
        .arg(summary)
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        // Reaped off-thread so a slow notification daemon never leaves a zombie
        // nor stalls the UI tick.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => log::debug!("notify-send unavailable: {e}"),
    }
}
