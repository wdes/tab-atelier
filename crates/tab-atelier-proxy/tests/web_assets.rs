// SPDX-License-Identifier: MPL-2.0

//! The committed UI bundle must match the TypeScript it came from.
//!
//! `assets/*.js` is generated from `web/src/*.ts` and committed, so that
//! building a `.deb` never needs Node. The cost of that choice is exactly one
//! failure mode — someone edits the `.ts` and forgets to rebuild, or edits the
//! generated `.js` directly and has it overwritten later — and this is what
//! closes it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `tsc` from the web toolchain, if it has been installed.
fn tsc(web: &Path) -> Option<PathBuf> {
    let p = web.join("node_modules/.bin/tsc");
    p.exists().then_some(p)
}

#[test]
fn the_committed_ui_bundle_matches_its_typescript() {
    let web = crate_dir().join("web");
    let assets = crate_dir().join("assets");
    let Some(tsc) = tsc(&web) else {
        // Not installed is not a failure: the whole point of committing the
        // output is that a build works without this toolchain. CI installs it;
        // a contributor building a package does not have to.
        eprintln!(
            "skipping: {}/node_modules not installed (`bun install` there to enable)",
            web.display()
        );
        return;
    };

    let out = tempdir("ta-web-build");
    let status = Command::new(&tsc)
        .args(["--outDir", &out.display().to_string()])
        .current_dir(&web)
        .status()
        .expect("run tsc");
    assert!(status.success(), "tsc failed — the sources do not compile");

    for name in ["app.js", "charts.js"] {
        let fresh = std::fs::read_to_string(out.join(name)).expect("compiler output");
        let committed = std::fs::read_to_string(assets.join(name)).expect("committed asset");
        assert_eq!(
            fresh.trim_end(),
            committed.trim_end(),
            "assets/{name} is stale. Run `bun run build` in crates/tab-atelier-proxy/web \
             and commit the result — and edit web/src/*.ts, never assets/*.js."
        );
    }
    let _ = std::fs::remove_dir_all(&out);
}

/// Type errors are a test failure, not a thing to notice later.
#[test]
fn the_ui_sources_type_check() {
    let web = crate_dir().join("web");
    let Some(tsc) = tsc(&web) else {
        eprintln!("skipping: web/node_modules not installed");
        return;
    };
    let out = Command::new(&tsc)
        .arg("--noEmit")
        .current_dir(&web)
        .output()
        .expect("run tsc");
    assert!(
        out.status.success(),
        "the UI sources do not type-check:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}
