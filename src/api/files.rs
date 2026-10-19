// SPDX-License-Identifier: MPL-2.0

//! File transfer inside a tab's sandbox: upload into `inbox/` (atomic,
//! symlink-guarded, per-token concurrency cap) and download from `inbox/` /
//! `outbox/` (sandbox-resolved, no-sniff).

use std::sync::{Arc, Mutex};

use log::info;
use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Reply, Request, cached};

use super::{
    DOWNLOAD_GZIP_MAX, TabSnapshot, UPLOAD_MAX_BYTES, UPLOAD_MAX_BYTES_MIB, UPLOAD_MAX_INFLIGHT_PER_TOKEN, UploadSlot,
    parse_tab_key, resolve_sandbox_path, resolve_tab_idx, sanitize_basename, write_new_file_no_symlink,
};

/// Where an upload landed.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Stored {
    /// Absolute, for a caller on the same host.
    path: String,
    /// Relative to the tab's cwd, for anything addressable through this API.
    relpath: String,
    bytes: usize,
}

pub(super) fn upload(
    request: &Request<'_>,
    state: &Arc<Mutex<TabSnapshot>>,
    provided_token: Option<&str>,
) -> Result<Doc<Stored>, Failure> {
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
    let _slot = UploadSlot::try_acquire(upload_token).map_err(|n| {
        Failure::problem(
            429,
            format!("too many concurrent uploads from this token ({n} already in flight; cap {UPLOAD_MAX_INFLIGHT_PER_TOKEN})"),
        )
    })?;
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/files") else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let Some(t) = snap.tabs.get(idx) else {
        return Err(Failure::problem(404, "tab index out of range"));
    };
    // Refuse uploads to a locked tab — same policy as POST
    // /input. Lock means "this tab is read-only right now";
    // a share recipient shouldn't be able to drop files
    // into the agent's inbox while the operator has paused
    // the session. `effective_locked()` covers BOTH the
    // manual flag and the off-hours schedule.
    if crate::schedule::LockState::effective_locked(t) {
        return Err(Failure::problem(423, "tab is locked"));
    }
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        return Err(Failure::problem(400, "tab has no known cwd"));
    };
    let Some(name) = request.query_value("name").and_then(sanitize_basename) else {
        return Err(Failure::problem(400, "missing or invalid ?name=<basename>"));
    };
    // Hard cap. The Content-Length pre-check already 413'd
    // anything bigger (see UPLOAD_MAX_BYTES below), so this
    // is the post-read safety net for `Transfer-Encoding:
    // chunked` requests we can't size in advance.
    let body_bytes = request.body;
    if body_bytes.len() > UPLOAD_MAX_BYTES {
        return Err(Failure::problem(
            413,
            format!("upload exceeds {UPLOAD_MAX_BYTES_MIB} MiB limit"),
        ));
    }
    let inbox = std::path::Path::new(&*cwd).join("inbox");
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        return Err(Failure::problem(500, format!("mkdir inbox: {e}")));
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
        return Err(Failure::problem(404, "inbox path unreadable"));
    };
    if !inbox_canon.starts_with(&cwd_canon) || inbox_canon.file_name() != Some(std::ffi::OsStr::new("inbox")) {
        return Err(Failure::problem(403, "inbox escapes the tab's cwd"));
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
        return Err(Failure::problem(500, format!("write inbox/.{name}.tmp: {e}")));
    }
    // rename() replaces the destination entry itself (it does
    // not follow a symlink at `dest`), so the rename can't be
    // redirected either.
    if let Err(e) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_file(&staging);
        return Err(Failure::problem(500, format!("rename into inbox/{name}: {e}")));
    }
    info!("API: stored {} bytes in {}", body_bytes.len(), dest.display());
    Ok(Doc::at(
        201,
        Stored {
            path: dest.to_string_lossy().into_owned(),
            relpath: format!("inbox/{name}"),
            bytes: body_bytes.len(),
        },
    ))
}

/// `GET /tabs/{key}/outbox/{name}` — the same download, addressed by path.
///
/// The viewer used `…/files?path=outbox/x.md`, whose LAST URL SEGMENT is
/// `files`. An `<a download>` only applies same-origin, so the moment the page
/// and the API differ (a share link, a tunnel), the browser ignores the
/// attribute and names the file after the URL — hence `files.bin`, the segment
/// plus an extension guessed from `application/octet-stream`.
///
/// With the name in the path there is nothing left to guess: the fallback name
/// IS the filename, `Content-Disposition` agrees with it, and a plain
/// `<a href download>` needs no JavaScript to work.
pub(super) fn download_by_path(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Reply, Failure> {
    // `/tabs/<key>/outbox/<rel>` → key, and `outbox/<rel>` as the sandbox path.
    let Some((left, rest)) = request.path.split_once("/outbox/") else {
        return Err(Failure::problem(404, "not a file path"));
    };
    let rel = super::url_decode(rest);
    if rel.is_empty() {
        return Err(Failure::problem(404, "no file named"));
    }
    // Reuse the query-form handler so the sandbox check, the ETag and the
    // Content-Disposition are the same code — a second copy of a path check
    // is a second chance to get it wrong.
    //
    // The handler below is path-shaped (`…/files?path=…`), so the suffix is
    // re-attached here rather than passing a second "what to serve" argument
    // through it.
    serve_path(state, &format!("{left}/files"), &format!("outbox/{rel}"), request)
}

pub(super) fn download(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Reply, Failure> {
    // Download a file from the tab's sandbox. `?path=…` must
    // resolve inside one of `FILE_SANDBOX_DIRS` (currently
    // `inbox/` + `outbox/`) of the tab's cwd — anything
    // else is rejected before any filesystem access. See
    // `resolve_sandbox_path` for the full check.
    let Some(raw_path) = request.query_value("path") else {
        return Err(Failure::problem(400, "missing ?path=<relative-path>"));
    };
    // Percent-decoded here, not by the caller: the dispatcher used to decode
    // this query value before handing it over, and a handler that reads the raw
    // query now owns that step. Without it an uploaded "Frédéric report.txt"
    // is looked up as the literal `Fr%C3%A9d%C3%A9ric%20report.txt` and 404s —
    // which is exactly what the RFC 5987 test caught.
    let raw_path = super::url_decode(raw_path);
    serve_path(state, request.path, &raw_path, request)
}

/// Resolve, guard and read one sandbox file, then answer it conditionally.
fn serve_path(
    state: &Arc<Mutex<TabSnapshot>>,
    path: &str,
    raw_path: &str,
    request: &Request<'_>,
) -> Result<Reply, Failure> {
    let Some((key_raw, is_uuid)) = parse_tab_key(path, "/files") else {
        return Err(Failure::problem(404, "invalid tab key"));
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
        return Err(Failure::problem(400, "tab has no known cwd"));
    };
    let canonical = resolve_sandbox_path(&cwd, raw_path).map_err(|(status, msg)| Failure::problem(status, msg))?;
    // Defense in depth against a component being swapped for a
    // symlink in the window between resolve_sandbox_path's
    // canonicalize and the read below: confirm the final entry
    // is still a regular file (not a symlink/dir/fifo) via an
    // lstat that does NOT follow links. Narrows the TOCTOU and
    // avoids reading through a freshly-planted symlink.
    let Ok(meta) = std::fs::symlink_metadata(&canonical) else {
        return Err(Failure::problem(404, "file not found"));
    };
    if !meta.file_type().is_file() {
        return Err(Failure::problem(403, "not a regular file"));
    }
    // Generic message — do not echo the absolute server path /
    // OS error back to a remote share-link holder.
    let Ok(bytes) = std::fs::read(&canonical) else {
        return Err(Failure::problem(404, "file not found"));
    };
    let display_name = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download");
    info!("API: served {} bytes from {}", bytes.len(), canonical.display());
    // See DOWNLOAD_GZIP_MAX — no gzip for big binary downloads.
    let client = tab_atelier_api::Conditional {
        accept_gzip: request.conditional.accept_gzip && bytes.len() <= DOWNLOAD_GZIP_MAX,
        if_none_match: request.conditional.if_none_match,
    };
    // RFC 5987 `filename*=UTF-8''…` so accented / non-ASCII
    // names ("Frédéric.txt") survive transit; the ASCII
    // fallback `filename="…"` is also included for legacy
    // user-agents.
    let mut percent: String = String::with_capacity(display_name.len());
    for byte in display_name.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            percent.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut percent, "%{byte:02X}");
        }
    }
    let ascii_fallback: String = display_name
        .chars()
        .filter(|c| c.is_ascii() && *c != '"' && *c != '\\')
        .collect();
    let etag = format!("{:08x}", crate::crc32(&bytes));
    Ok(cached(
        200,
        "application/octet-stream",
        bytes::Bytes::from(bytes),
        &etag,
        client,
    )
    .with_header(
        "content-disposition",
        format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{percent}"),
    )
    // `nosniff` so a browser never second-guesses `application/octet-stream`
    // from the file's leading bytes — an uploaded HTML file must download, not
    // render as a page on this origin.
    .with_header("x-content-type-options", "nosniff".to_owned()))
}
