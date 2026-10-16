// SPDX-License-Identifier: MPL-2.0

//! Serving a UI from a web root on disk — the alternative to embedding it.
//!
//! The UI is files beside the binary, read per request and served as they are.
//! Not `include_str!`: an embedded UI makes every CSS tweak a rebuild of the
//! server, and a large one puts megabytes of string literals in the binary and
//! in every debug build.

use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use http::{Response, StatusCode};

use crate::response::{self, Body};

/// The web root for a UI: `$env_var` if set, else `default`, if it is a
/// directory. `None` otherwise, so the caller reports the missing UI once at
/// startup instead of 404ing asset after asset.
#[must_use]
pub fn web_root(env_var: &str, default: &str) -> Option<PathBuf> {
    let dir = std::env::var(env_var).map_or_else(|_| PathBuf::from(default), PathBuf::from);
    dir.is_dir().then_some(dir)
}

/// The content type for a path, by extension. A short list, not a MIME table —
/// the set a hand-written UI actually ships.
#[must_use]
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// The request-relative path, or `None` if it is not one.
///
/// The path arrives from the URL, so it is checked **before** it is joined to
/// the root: no `..`, no absolute segment, nothing that walks out of the tree.
/// The guard lives here once, not at every call site.
#[must_use]
pub fn safe_rel(rel: &str) -> Option<PathBuf> {
    let trimmed = rel.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            // ParentDir, RootDir and Prefix can each leave the tree.
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Read `rel` under `root`: its content type and its bytes, or `None`.
///
/// The half of [`serve`] that is about *finding* a UI file rather than about
/// shaping an HTTP response. A server whose framework builds its own response —
/// Rocket, say — takes this and still gets the one traversal guard and the one
/// content-type table, instead of re-deriving both and drifting.
#[must_use]
pub fn read(root: &Path, rel: &str) -> Option<(&'static str, Bytes)> {
    let safe = safe_rel(rel)?;
    let path = root.join(safe);
    let data = std::fs::read(&path).ok()?;
    Some((content_type(&path), Bytes::from(data)))
}

/// Read `rel` under `root` and answer with it.
///
/// `None` when the path is not a safe relative one, or the file is not there —
/// the caller turns that into a 404.
#[must_use]
pub fn serve(root: &Path, rel: &str) -> Option<Response<Body>> {
    let (content_type, data) = read(root, rel)?;
    Some(response::bytes(StatusCode::OK, content_type, data))
}

#[cfg(test)]
mod tests {
    use super::{content_type, read, safe_rel, serve};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ta-web-assets-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make dir");
        dir
    }

    /// Content types cover the files a UI ships, and default sanely.
    #[test]
    fn content_types_cover_the_ui_files() {
        assert!(content_type(Path::new("index.html")).starts_with("text/html"));
        assert!(content_type(Path::new("app.js")).starts_with("text/javascript"));
        assert!(content_type(Path::new("app.css")).starts_with("text/css"));
        assert_eq!(content_type(Path::new("data.json")), "application/json");
        assert_eq!(content_type(Path::new("logo.png")), "image/png");
        assert_eq!(content_type(Path::new("weird.bin")), "application/octet-stream");
    }

    /// The traversal guard is the point: a URL path cannot walk out of the root.
    #[test]
    fn a_relative_path_cannot_leave_the_tree() {
        assert_eq!(safe_rel("/assets/app.js"), Some(PathBuf::from("assets/app.js")));
        assert_eq!(safe_rel("app.js"), Some(PathBuf::from("app.js")));
        assert_eq!(safe_rel("a/./b.js"), Some(PathBuf::from("a/b.js")), "a `.` is dropped");
        // A leading `/` is the URL's own, so it is trimmed: `/abs/root` is the
        // in-root path `abs/root`, not an escape.
        assert_eq!(safe_rel("/abs/root"), Some(PathBuf::from("abs/root")));
        for bad in ["", "/", "../../etc/passwd", "a/../../b"] {
            assert_eq!(safe_rel(bad), None, "{bad:?} must not resolve");
        }
    }

    /// A file under the root is served with its type; anything else is `None`.
    #[test]
    fn serve_reads_within_the_root_only() {
        let root = scratch("serve");
        fs::write(root.join("app.js"), "console.log(1)").expect("write");
        fs::write(root.join("secret.txt"), "nope").expect("write a sibling");

        let ok = serve(&root, "app.js").expect("served");
        assert_eq!(ok.headers()["content-type"], "text/javascript; charset=utf-8");

        assert!(serve(&root, "missing.js").is_none(), "a missing file is a 404");
        assert!(serve(&root, "../secret.txt").is_none(), "and so is an escape");
    }

    /// The primitive under [`serve`]: the same guard and the same table, without
    /// shaping a response — what a framework-driven server builds on.
    #[test]
    fn read_gives_the_type_and_the_bytes_under_the_same_guard() {
        let root = scratch("read");
        fs::write(root.join("page.html"), "<html></html>").expect("write");

        let (content_type, bytes) = read(&root, "page.html").expect("read");
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert_eq!(bytes, "<html></html>");

        // The guard is `serve`'s, because it is the same call.
        assert!(read(&root, "../page.html").is_none(), "an escape is refused");
        assert!(read(&root, "").is_none(), "and so is an empty path");
    }
}
