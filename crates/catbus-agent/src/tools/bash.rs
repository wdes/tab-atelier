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
        //
        // The offset comes from `text::tail_start` rather than being `len - MAX_OUTPUT`, which is a
        // byte count where the string is characters: a cut inside a multi-byte character made
        // `split_off` panic, and a command printing enough accented or box-drawing text is all it
        // takes. That panic ended a live turn.
        let start = crate::text::tail_start(&combined, MAX_OUTPUT);
        let tail = combined.split_off(start);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Output past the cap must be cut down without panicking on a character boundary.
    ///
    /// This reproduces a crash from a live session: the model ran a grep, the output was over the
    /// 256 KB cap, and the cut landed inside a multi-byte character — which `String::split_off`
    /// treats as a programming error and asserts against, ending the turn with
    /// `assertion failed: self.is_char_boundary(at)`.
    ///
    /// The character width is chosen rather than picked: an em dash is three bytes, and
    /// `MAX_OUTPUT` is `0b100_0000_0000_0000`, whose remainder modulo three is one — so
    /// `len - MAX_OUTPUT` lands two bytes *into* a character rather than at its start. Two-byte
    /// characters would not do it: the offset stays even and even offsets are all boundaries, so the
    /// test would pass whether or not the bug was there. That is asserted below, so the test cannot
    /// quietly stop reproducing if the cap changes.
    #[tokio::test]
    async fn output_past_the_cap_is_cut_without_splitting_a_character() {
        // Written by us and `cat`ed rather than generated by `printf`: the bytes have to be exactly
        // the ones this test reasons about, and `\u` handling in the shell is one more thing that
        // could differ.
        let file = std::env::temp_dir().join(format!("catbus-bash-cap-{}.txt", std::process::id()));
        let content = "\u{2014}".repeat(100_000); // 300_000 bytes, three per character
        std::fs::write(&file, &content).expect("write the large file");

        // The premise: this input is over the cap, and the arithmetic the bug used would have landed
        // inside a character. If either stops being true the test is no longer a reproduction, and
        // it should say so rather than pass.
        assert!(content.len() > MAX_OUTPUT, "the input must be over the cap");
        assert!(
            !content.is_char_boundary(content.len() - MAX_OUTPUT),
            "the input must put the old byte-counted cut inside a character, or this proves nothing"
        );

        let command = format!("cat {}", file.display());
        let out = run(&serde_json::json!({ "command": command }), Path::new("/tmp"))
            .await
            .expect("a large output should be trimmed, not fail");
        let _ = std::fs::remove_file(&file);

        assert!(out.starts_with("[...truncated...]"), "the cut should be marked");
        assert!(out.len() <= MAX_OUTPUT + 32, "the tail should be the capped size");
        // The tail is still the output, whole and undamaged. A lossy decode would have shown its
        // damage as replacement characters instead.
        assert!(out.contains('\u{2014}'), "the tail should hold the output itself");
        assert!(out.is_char_boundary(out.len()));
    }

    /// A command whose output fits is returned whole, with no marker.
    #[tokio::test]
    async fn output_under_the_cap_is_untouched() {
        let command = "printf 'small \u{e9} output\\n'";
        let out = run(&serde_json::json!({ "command": command }), Path::new("/tmp"))
            .await
            .expect("a small output should come back");
        assert_eq!(out, "small \u{e9} output\n");
    }
}
