// SPDX-License-Identifier: MPL-2.0

//! The `/outbox` + `/inbox` listing: the file tree the share viewer renders
//! for its download / sent-files panels.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::{TabSnapshot, collect_files_tree, parse_tab_key, resolve_tab_idx};

/// One file in the tree.
///
/// A type rather than a `serde_json::Value`, which is what this was: the shape
/// is fixed, so building it as an untyped map meant the sort below had to treat
/// a missing `path` as `""` and the response could not be documented.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FileEntry {
    /// The basename, already sanitised.
    pub name: String,
    /// Relative to the listing's `dir`, so a download resolves it against
    /// `<dir>/<path>` and cannot climb out.
    pub path: String,
    /// Bytes.
    pub size: u64,
    /// Unix seconds of the last modification.
    pub mtime: u64,
}

/// The listing, or an empty one when the tab has no cwd or the directory is
/// not inside it.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Tree {
    files: Vec<FileEntry>,
    dir: String,
}

pub(super) fn list(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Tree>, Failure> {
    let dirname = if request.path.ends_with("/outbox") {
        "outbox"
    } else {
        "inbox"
    };
    let suffix = if dirname == "outbox" { "/outbox" } else { "/inbox" };
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, suffix) else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let empty = || {
        Doc::ok(Tree {
            files: Vec::new(),
            dir: String::new(),
        })
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let Some(t) = snap.tabs.get(idx) else {
        return Err(Failure::problem(404, "tab index out of range"));
    };
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        return Ok(empty());
    };
    let dir_path = std::path::Path::new(&*cwd).join(dirname);
    // The DIRECTORY itself must not be a way out. `read_dir` follows a symlink
    // given as the path, so an `outbox -> /` link (which anything running in
    // the tab can create) turned this listing into a filesystem browser for
    // anyone holding a read-only share token. Downloads were never exposed —
    // `resolve_sandbox_path` canonicalises — but the names, sizes and mtimes
    // of the whole host were. Same check here, so the two agree.
    let inside = match (std::path::Path::new(&*cwd).canonicalize(), dir_path.canonicalize()) {
        (Ok(root), Ok(real)) => real.starts_with(&root),
        // Not created yet is normal and lists as empty; anything else is
        // refused rather than guessed at.
        _ => false,
    };
    if !inside {
        return Ok(empty());
    }
    // Walk the whole subtree (not just the top level) so files the
    // agent tucked into subfolders show up — the viewer renders
    // them in tree mode. Each file carries a `path` relative to
    // `dir_path`; downloads resolve it against `<dir>/<path>`.
    let mut files = Vec::new();
    collect_files_tree(&dir_path, "", 0, &mut files);
    // Stable order (by relative path) so folders group together and
    // the viewer's diff (new-file toast) is predictable across polls. On the
    // typed entries this is a plain string compare with no "missing field"
    // case to worry about.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Doc::ok(Tree {
        files,
        dir: dir_path.to_string_lossy().into_owned(),
    }))
}
