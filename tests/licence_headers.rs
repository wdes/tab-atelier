// SPDX-License-Identifier: MPL-2.0

// Integration test crate — `.unwrap()` is idiomatic here (the crate-wide deny
// in Cargo.toml also covers `tests/`, which never sets `cfg(test)`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Every source file declares its licence in a form a scanner can read.
//!
//! This repo ships as a Debian package, and `debian/copyright` is derived by
//! running `licensecheck` over the tree. A header the scanner cannot parse is
//! not a cosmetic problem there: the file comes back `License: UNKNOWN` with a
//! `FIXME`, which is the kind of thing that gets a package bounced.
//!
//! The guard exists because the tree spent a commit in exactly that state. The
//! three-line MPL notice was collapsed to one line — a good change — but the
//! line written was `// @licence MPL-2.0 <url>`, a spelling no scanner knows,
//! so 149 files went from *detected as MPL-2.0* to *UNKNOWN* in one commit.
//! Nothing noticed, because nothing looks: there is no `reuse`, no
//! `licensecheck` and no SPDX step anywhere in `.github/workflows/`.
//!
//! `SPDX-License-Identifier:` is the tag every tool in that chain agrees on,
//! and it is no longer than what it replaced.

use std::path::{Path, PathBuf};

/// The tag, split so this file's own header is not what satisfies the search
/// when the test reads the tree.
const TAG: &str = "SPDX-License-Identifier:";
const LICENCE: &str = "MPL-2.0";

/// Extensions that carry a header, and the comment syntax each uses.
fn comment_marker(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" | "js" => Some("//"),
        "sh" | "py" => Some("#"),
        // HTML is shipped and served, so it is labelled like the rest.
        //
        // Markdown deliberately is NOT. Prose documentation is not what
        // licensecheck is run over to build debian/copyright, and only one
        // .md in the tree ever carried a header — one file is an outlier,
        // not a convention to enforce across every doc in docs/.
        "html" => Some("<!--"),
        _ => None,
    }
}

/// Paths that are not ours to label.
fn skipped(path: &Path) -> bool {
    let s = path.to_string_lossy();
    // Vendored libraries keep their upstream headers; `target` is build
    // output; the licence texts themselves are the real thing, not a notice
    // about it.
    s.contains("/target/")
        || s.contains("/vendor/")
        || s.contains("/node_modules/")
        || s.contains("/.git/")
        // Scratch space for inter-tab handoffs, not shipped source.
        || s.contains("/inbox/")
        || s.contains("/outbox/")
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if skipped(&path) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_source = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| comment_marker(e).is_some());
            if is_source {
                out.push(path);
            }
        }
    }
    out
}

/// A shebang, as the kernel understands it.
///
/// `#!/` and not `#!`: Rust's inner attributes (`#![allow(...)]`) start with
/// the same two characters, and treating those as shebangs made this test
/// report every file that has one as broken.
fn is_shebang(line: &str) -> bool {
    line.starts_with("#!/")
}

/// The lines a scanner reads before it gives up looking for a licence.
///
/// Not strictly "line 1": two things legitimately come first and cannot be
/// moved. A shebang must be on line 1 or the kernel ignores it, and
/// `<!doctype html>` must precede everything or the browser falls into quirks
/// mode. So the tag has to be near the top, not at the very top —
/// `licensecheck` reads a chunk of the head for the same reason.
const HEADER_LINES: usize = 3;

/// The first line that is not a shebang, a doctype, or blank.
fn first_content_line(src: &str) -> &str {
    src.lines()
        .find(|l| {
            let t = l.trim();
            !is_shebang(l) && !t.is_empty() && !t.to_ascii_lowercase().starts_with("<!doctype")
        })
        .unwrap_or("")
}

/// Whether the licence tag sits in the header region.
fn declares_licence(src: &str) -> bool {
    src.lines()
        .filter(|l| {
            let t = l.trim();
            !is_shebang(l) && !t.is_empty() && !t.to_ascii_lowercase().starts_with("<!doctype")
        })
        .take(HEADER_LINES)
        .any(|l| l.contains(TAG))
}

#[test]
fn every_source_file_declares_its_licence_where_a_scanner_looks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = source_files(root);
    assert!(
        files.len() > 100,
        "only found {} source files — the walk is not reaching the tree",
        files.len()
    );

    let mut missing = Vec::new();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if !declares_licence(&src) {
            missing.push(format!(
                "{}: first line is {:?}",
                path.strip_prefix(root).unwrap_or(path).display(),
                first_content_line(&src)
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "these do not carry `{TAG} {LICENCE}` on their first non-shebang line, so \
         licensecheck reports them as UNKNOWN and debian/copyright inherits that:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn the_shebang_still_comes_first_where_there_is_one() {
    // The one way this could have broken loudly rather than silently: a
    // licence line above `#!/usr/bin/env bash` stops the script being
    // executable at all. Checked separately from the header itself because
    // the failure is so much worse than a mislabelled file.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut wrong = Vec::new();
    let mut checked = 0;
    for path in source_files(root) {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Only files that HAVE a shebang somewhere near the top are in scope.
        if !src.lines().take(5).any(is_shebang) {
            continue;
        }
        checked += 1;
        if !src.starts_with("#!/") {
            wrong.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
        }
    }
    assert!(
        checked > 5,
        "only found {checked} scripts with a shebang — did they move?"
    );
    assert!(
        wrong.is_empty(),
        "these have a shebang that is not on line 1, so the kernel will not honour it:\n  {}",
        wrong.join("\n  ")
    );
}

#[test]
fn the_licence_named_in_the_headers_is_the_one_the_repo_ships() {
    // A header claiming a licence the repo does not carry is worse than none:
    // it is a confident wrong answer, and it is what debian/copyright would
    // inherit.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo_toml = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
    assert!(
        cargo_toml.contains(&format!("license = \"{LICENCE}\"")),
        "the headers say {LICENCE} but Cargo.toml does not"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("src/lib.rs");
    assert!(
        first_content_line(&lib).contains(&format!("{TAG} {LICENCE}")),
        "src/lib.rs does not carry the exact tag the rest of the tree does"
    );
}
