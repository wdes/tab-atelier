// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! File transfer inside a tab's sandbox: upload into `inbox/` (atomic,
//! symlink-guarded, per-token concurrency cap) and download from `inbox/` /
//! `outbox/` (sandbox-resolved, no-sniff).

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::{
    DOWNLOAD_GZIP_MAX, TabSnapshot, UPLOAD_MAX_BYTES, UPLOAD_MAX_BYTES_MIB, UPLOAD_MAX_INFLIGHT_PER_TOKEN, UploadSlot,
    error_json, parse_tab_key, resolve_sandbox_path, resolve_tab_idx, respond_json, respond_with_etag,
    sanitize_basename, write_new_file_no_symlink,
};

pub(super) fn upload<W: Write>(
    stream: &mut W,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    body_bytes: &[u8],
    provided_token: Option<&str>,
    query_name: Option<&str>,
) {
    // Upload file body into the tab's `cwd/inbox/<name>`.
    // `?name=<basename>` is required and is sanitised to a
    // path-component (no `..`, no separators) so a malicious
    // remote can't write outside `inbox/`. Accepts both
    // `/tabs/<idx>/files` and `/tabs/by-id/<uuid>/files`
    // forms; share-token auth (rw only) was vetted upstream.
    // Per-token concurrency cap: refuse with 429 when N
    // uploads are already in flight from this same token, so
    // one share recipient can't queue dozens of concurrent
    // 100 MiB POSTs and amplify memory pressure (audit #3).
    let upload_token = provided_token.unwrap_or("");
    let _slot = match UploadSlot::try_acquire(upload_token) {
        Ok(s) => s,
        Err(n) => {
            error_json(
                stream,
                429,
                &format!(
                    "too many concurrent uploads from this token ({n} already in flight; cap {UPLOAD_MAX_INFLIGHT_PER_TOKEN})"
                ),
            );
            return;
        }
    };
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
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
    // Refuse uploads to a locked tab — same policy as POST
    // /input. Lock means "this tab is read-only right now";
    // a share recipient shouldn't be able to drop files
    // into the agent's inbox while the operator has paused
    // the session. `effective_locked()` covers BOTH the
    // manual flag and the off-hours schedule.
    if crate::schedule::LockState::effective_locked(t) {
        drop(snap);
        error_json(stream, 423, "tab is locked");
        return;
    }
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        error_json(stream, 400, "tab has no known cwd");
        return;
    };
    let Some(name) = query_name.and_then(sanitize_basename) else {
        error_json(stream, 400, "missing or invalid ?name=<basename>");
        return;
    };
    // Hard cap. The Content-Length pre-check already 413'd
    // anything bigger (see UPLOAD_MAX_BYTES below), so this
    // is the post-read safety net for `Transfer-Encoding:
    // chunked` requests we can't size in advance.
    if body_bytes.len() > UPLOAD_MAX_BYTES {
        error_json(stream, 413, &format!("upload exceeds {UPLOAD_MAX_BYTES_MIB} MiB limit"));
        return;
    }
    let inbox = std::path::Path::new(&*cwd).join("inbox");
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        error_json(stream, 500, &format!("mkdir inbox: {e}"));
        return;
    }
    // Sandbox guard (parity with the GET /files download path,
    // which funnels through resolve_sandbox_path). The upload
    // path used to `std::fs::write` straight into `cwd/inbox`
    // with no symlink check, so a symlinked `inbox` (or a
    // symlink planted at the destination) could redirect the
    // write to an arbitrary file. Canonicalise and confirm the
    // resolved inbox is a real directory *inside* the tab's cwd
    // whose final component is still `inbox`.
    let resolved = std::path::Path::new(&*cwd)
        .canonicalize()
        .ok()
        .zip(inbox.canonicalize().ok());
    let Some((cwd_canon, inbox_canon)) = resolved else {
        error_json(stream, 404, "inbox path unreadable");
        return;
    };
    if !inbox_canon.starts_with(&cwd_canon) || inbox_canon.file_name() != Some(std::ffi::OsStr::new("inbox")) {
        error_json(stream, 403, "inbox escapes the tab's cwd");
        return;
    }
    // Atomic write: stage to <name>.tmp then rename. A reader
    // walking inbox/ never sees a half-written file. `create_new`
    // (O_EXCL) refuses to create *through* a symlink, so a
    // pre-planted symlink at the staging name can't redirect the
    // write — we drop any stale entry (incl. a symlink) first so
    // the exclusive create lands fresh.
    let dest = inbox_canon.join(&name);
    let staging = inbox_canon.join(format!(".{name}.tmp"));
    if let Err(e) = write_new_file_no_symlink(&staging, body_bytes) {
        error_json(stream, 500, &format!("write inbox/.{name}.tmp: {e}"));
        return;
    }
    // rename() replaces the destination entry itself (it does
    // not follow a symlink at `dest`), so the rename can't be
    // redirected either.
    if let Err(e) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_file(&staging);
        error_json(stream, 500, &format!("rename into inbox/{name}: {e}"));
        return;
    }
    info!("API: stored {} bytes in {}", body_bytes.len(), dest.display());
    let body = serde_json::to_string(&serde_json::json!({
        "path": dest.to_string_lossy(),
        "relpath": format!("inbox/{name}"),
        "bytes": body_bytes.len(),
    }))
    .unwrap_or_default();
    respond_json(stream, 201, &body);
}

pub(super) fn download<W: Write>(
    stream: &mut W,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    query_path: Option<&str>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    // Download a file from the tab's sandbox. `?path=…` must
    // resolve inside one of `FILE_SANDBOX_DIRS` (currently
    // `inbox/` + `outbox/`) of the tab's cwd — anything
    // else is rejected before any filesystem access. See
    // `resolve_sandbox_path` for the full check.
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
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
        error_json(stream, 400, "tab has no known cwd");
        return;
    };
    let Some(raw_path) = query_path else {
        error_json(stream, 400, "missing ?path=<relative-path>");
        return;
    };
    let canonical = match resolve_sandbox_path(&cwd, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => {
            error_json(stream, status, &msg);
            return;
        }
    };
    // Defense in depth against a component being swapped for a
    // symlink in the window between resolve_sandbox_path's
    // canonicalize and the read below: confirm the final entry
    // is still a regular file (not a symlink/dir/fifo) via an
    // lstat that does NOT follow links. Narrows the TOCTOU and
    // avoids reading through a freshly-planted symlink.
    let Ok(meta) = std::fs::symlink_metadata(&canonical) else {
        error_json(stream, 404, "file not found");
        return;
    };
    if !meta.file_type().is_file() {
        error_json(stream, 403, "not a regular file");
        return;
    }
    // Generic message — do not echo the absolute server path /
    // OS error back to a remote share-link holder.
    let Ok(bytes) = std::fs::read(&canonical) else {
        error_json(stream, 404, "file not found");
        return;
    };
    let display_name = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download");
    info!("API: served {} bytes from {}", bytes.len(), canonical.display());
    // See DOWNLOAD_GZIP_MAX — no gzip for big binary downloads.
    let accept_gzip = accept_gzip && bytes.len() <= DOWNLOAD_GZIP_MAX;
    // RFC 5987 `filename*=UTF-8''…` so accented / non-ASCII
    // names ("Frédéric.txt") survive transit; the ASCII
    // fallback `filename="…"` is also included for legacy
    // user-agents.
    let mut percent: String = String::with_capacity(display_name.len());
    for byte in display_name.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            percent.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut percent, "%{byte:02X}");
        }
    }
    let ascii_fallback: String = display_name
        .chars()
        .filter(|c| c.is_ascii() && *c != '"' && *c != '\\')
        .collect();
    let disposition = format!(
        "Content-Disposition: attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{percent}\r\nX-Content-Type-Options: nosniff\r\n"
    );
    respond_with_etag(
        stream,
        200,
        "application/octet-stream",
        &bytes,
        accept_gzip,
        if_none_match,
        &disposition,
    );
}
