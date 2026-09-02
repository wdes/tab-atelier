// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `/outbox` + `/inbox` listing: the file tree the share viewer renders
//! for its download / sent-files panels.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, collect_files_tree, error_json, parse_tab_key, resolve_tab_idx, respond_json};

pub(super) fn list<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let dirname = if p.ends_with("/outbox") { "outbox" } else { "inbox" };
    let suffix = if dirname == "outbox" { "/outbox" } else { "/inbox" };
    let Some((key_raw, is_uuid)) = parse_tab_key(p, suffix) else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        drop(snap);
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        respond_json(stream, 200, r#"{"files":[],"dir":""}"#);
        return;
    };
    let dir_path = std::path::Path::new(&*cwd).join(dirname);
    // Walk the whole subtree (not just the top level) so files the
    // agent tucked into subfolders show up — the viewer renders
    // them in tree mode. Each file carries a `path` relative to
    // `dir_path`; downloads resolve it against `<dir>/<path>`.
    let mut files: Vec<serde_json::Value> = Vec::new();
    collect_files_tree(&dir_path, "", 0, &mut files);
    // Stable order (by relative path) so folders group together and
    // the viewer's diff (new-file toast) is predictable across polls.
    files.sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
    let body = serde_json::to_string(&serde_json::json!({
        "files": files,
        "dir": dir_path.to_string_lossy(),
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}
