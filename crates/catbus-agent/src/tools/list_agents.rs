// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Enumerate every catbus-agent currently listening on a socket. We
//! treat the on-disk socket file in `~/.claude/projects/*/*.sock` as
//! the directory of agents — any agent that's still alive holds an
//! exclusive bind on its socket file, so the file is both the address
//! to talk to it *and* a liveness proof.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[allow(clippy::unused_async)] // Symmetry with the other tool fns.
pub async fn run(_input: &serde_json::Value, _cwd: &Path) -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "no $HOME".to_string())?;
    let projects = PathBuf::from(home).join(".claude").join("projects");
    let mut found: Vec<(String, String, String)> = Vec::new(); // (session_id, cwd_label, socket_path)
    let Ok(read_dir) = std::fs::read_dir(&projects) else {
        return Ok("(no agents — ~/.claude/projects not readable)".to_string());
    };
    for entry in read_dir.flatten() {
        let project_dir = entry.path();
        let Ok(inner) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        // "cwd label" derived from the dir name back into a slashed
        // path is impossible without information loss (escape is
        // many-to-one), so show the encoded name as-is.
        let cwd_label = project_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for f in inner.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("sock") {
                continue;
            }
            let Some(session_id) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Live socket check: try to connect with a tight timeout.
            // A stale socket from a crashed agent stays on disk; the
            // connect attempt fails fast in that case.
            if !is_alive(&p) {
                continue;
            }
            found.push((session_id.to_string(), cwd_label.clone(), p.display().to_string()));
        }
    }
    if found.is_empty() {
        return Ok("(no agents currently running)".to_string());
    }
    let mut out = String::with_capacity(found.len() * 80);
    for (sid, cwd, sock) in &found {
        let _ = writeln!(out, "{sid}  cwd={cwd}\n    socket={sock}");
    }
    Ok(out)
}

/// Try to connect to the socket with a short timeout. Returns true
/// when an accept loop is on the other end.
fn is_alive(path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let Ok(stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    // We don't actually send anything — a successful connect means
    // there *is* a listener. The handshake (`{"kind":"started"}`)
    // happens automatically server-side but we don't read it; the
    // stream drops on return.
    drop(stream);
    true
}
