// SPDX-License-Identifier: MPL-2.0

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Default ceiling — most builds / test suites / curl-and-jq pipes
/// finish well under this. Real cargo compiles take longer; the model
/// can pass `timeout_secs` to override per-call.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_TIMEOUT: Duration = Duration::from_hours(1);
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

    let child = spawn(command, cwd)?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("wait: {e}")),
        Err(_) => return Err(format!("timed out after {}s", timeout.as_secs())),
    };
    let mut combined = bytes_to_string(out.stdout);
    if !out.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("--- stderr ---\n");
        combined.push_str(&bytes_to_string(out.stderr));
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

/// Start `command` with this crate's conventions, without waiting for it.
///
/// Split out of [`run`] so the REPL can run a command the same way the tool does
/// and still watch the output as it arrives: [`run`] has to wait for the whole
/// thing because the model wants one finished chunk, whereas the operator wants
/// to see it happening. The conventions themselves are not two decisions, so
/// they live in one place.
///
/// `bash -lc` so we inherit the user's PATH and aliases. `kill_on_drop` matters
/// to whoever holds the returned child: a `Child` does not kill on drop, so
/// without it a command the operator abandoned keeps running — together with
/// anything it started — with nothing left to reap it or report on it.
///
/// # Errors
/// Returns a description when the shell cannot be started at all.
pub fn spawn(command: &str, cwd: &Path) -> Result<tokio::process::Child, String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| format!("spawn bash: {e}"))
}

/// Move-friendly bytes → String conversion: for valid UTF-8 (the common
/// case for build logs / curl output) this hands the Vec's buffer
/// straight to the String without copying. Falls back to the lossy path
/// only when the bytes contain invalid sequences.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}
