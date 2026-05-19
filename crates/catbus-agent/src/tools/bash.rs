// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Default ceiling — most builds / test suites / curl-and-jq pipes
/// finish well under this. Real cargo compiles take longer; the model
/// can pass `timeout_secs` to override per-call.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);
const MAX_OUTPUT: usize = 256 * 1024;

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing command".to_string())?;
    let timeout = input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map(Duration::from_secs)
        .map_or(DEFAULT_TIMEOUT, |d| d.min(MAX_TIMEOUT));

    // `bash -lc` so we inherit the user's PATH / aliases. Stderr is
    // merged with stdout to give the model one chunk of context.
    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd.spawn().map_err(|e| format!("spawn bash: {e}"))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("wait: {e}")),
        Err(_) => return Err(format!("timed out after {}s", timeout.as_secs())),
    };
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&out.stdout));
    if !out.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("--- stderr ---\n");
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    if combined.len() > MAX_OUTPUT {
        // Keep the tail — typical when a build dumps thousands of
        // OK lines followed by the actual error.
        let tail = combined.split_off(combined.len() - MAX_OUTPUT);
        combined = format!("[...truncated...]\n{tail}");
    }
    if !out.status.success() {
        let _ = write!(combined, "\n[exit {}]", out.status.code().unwrap_or(-1));
    }
    Ok(combined)
}
