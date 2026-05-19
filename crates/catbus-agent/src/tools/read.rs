// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;
use std::path::Path;

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let offset = input.get("offset").and_then(serde_json::Value::as_u64).unwrap_or(1);
    let limit = input.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(2000);
    let full = super::resolve(cwd, path);
    let text = tokio::fs::read_to_string(&full)
        .await
        .map_err(|e| format!("read {}: {e}", full.display()))?;
    // 1-based offset, capped at file length. `take(limit)` gates how
    // much we ship back to the model in one go.
    let start = offset.saturating_sub(1) as usize;
    let mut out = String::with_capacity(text.len().min(64 * 1024));
    for (i, line) in text.lines().enumerate().skip(start).take(limit as usize) {
        // `cat -n` style numbering — matches the desktop Read tool's
        // output so the model can use the same conventions.
        let _ = writeln!(out, "{:>6}\t{line}", i + 1);
    }
    Ok(out)
}
