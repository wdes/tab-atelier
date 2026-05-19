// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let old_string = input
        .get("old_string")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing old_string".to_string())?;
    let new_string = input
        .get("new_string")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing new_string".to_string())?;
    if old_string == new_string {
        return Err("old_string and new_string are identical".to_string());
    }
    let full = super::resolve(cwd, path);
    let text = tokio::fs::read_to_string(&full)
        .await
        .map_err(|e| format!("read {}: {e}", full.display()))?;
    let occurrences = text.matches(old_string).count();
    if occurrences == 0 {
        return Err("old_string not found in file".to_string());
    }
    if occurrences > 1 {
        return Err(format!(
            "old_string appears {occurrences} times — add more context to make it unique"
        ));
    }
    let replaced = text.replacen(old_string, new_string, 1);
    tokio::fs::write(&full, &replaced)
        .await
        .map_err(|e| format!("write {}: {e}", full.display()))?;
    Ok(format!(
        "Edited {} ({} bytes → {} bytes)",
        full.display(),
        text.len(),
        replaced.len()
    ))
}
