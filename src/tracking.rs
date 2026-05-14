// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

const DEBOUNCE_SECS: u64 = 2;

pub const USER_AGENT: &str = concat!("swoop/", env!("CARGO_PKG_VERSION"), " (terminal; +https://github.com/wdes/swoop)");

pub enum HeartbeatEvent {
    Activity { cwd: Option<PathBuf> },
    Shutdown,
}

pub struct WakatimeTracker {
    tx: mpsc::Sender<HeartbeatEvent>,
}

impl WakatimeTracker {
    pub fn new(api_key: String) -> Self {
        let (tx, rx) = mpsc::channel::<HeartbeatEvent>();

        std::thread::spawn(move || {
            let mut last_sent: u64 = 0;
            let mut last_project: Option<String> = None;

            loop {
                match rx.recv() {
                    Ok(HeartbeatEvent::Activity { cwd }) => {
                        let now = unix_now();
                        let project = cwd.as_deref().and_then(detect_project);
                        let project_changed = project != last_project;

                        if now - last_sent < DEBOUNCE_SECS && !project_changed {
                            continue;
                        }

                        last_sent = now;
                        last_project = project.clone();
                        let _ = send_heartbeat(&api_key, now, project.as_deref());
                    }
                    Ok(HeartbeatEvent::Shutdown) | Err(_) => break,
                }
            }
        });

        Self { tx }
    }

    pub fn record_activity(&self, cwd: Option<PathBuf>) {
        let _ = self.tx.send(HeartbeatEvent::Activity { cwd });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(HeartbeatEvent::Shutdown);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

fn send_heartbeat(api_key: &str, time: u64, project: Option<&str>) -> Result<(), String> {
    let mut body = serde_json::json!({
        "entity": "swoop-terminal",
        "type": "app",
        "time": time as f64,
        "category": "coding",
    });

    if let Some(p) = project {
        body["project"] = serde_json::json!(p);
    }

    let resp = ureq::post("https://api.wakatime.com/api/v1/users/current/heartbeats")
        .header("User-Agent", USER_AGENT)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes());

    match resp {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("wakatime: {e}")),
    }
}
