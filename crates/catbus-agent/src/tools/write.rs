// SPDX-License-Identifier: MPL-2.0

use std::path::Path;

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let content = input
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing content".to_string())?;
    let full = super::resolve(cwd, path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    tokio::fs::write(&full, content)
        .await
        .map_err(|e| format!("write {}: {e}", full.display()))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), full.display()))
}
