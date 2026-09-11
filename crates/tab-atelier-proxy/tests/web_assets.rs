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

/// The dashboard's window dropdown must offer exactly what the server parses.
///
/// This is the one piece of the UI with a wire contract in it: the `<option>`
/// values are `usage::Window` tokens, and a token renamed on one side alone
/// gives a dropdown that 400s or silently falls back to the default. Neither
/// shows up as a compile error and neither is loud at runtime — the graph just
/// draws the wrong range.
///
/// Read out of the committed HTML rather than the TypeScript, because the
/// markup is where the options live and the markup is not compiled.
#[test]
fn the_window_dropdown_matches_the_windows_the_server_parses() {
    let html = std::fs::read_to_string(crate_dir().join("assets/index.html")).expect("index.html");

    let values = select_values(&html, "usageWindow");
    assert!(!values.is_empty(), "no usageWindow <select> found in index.html");

    // Every offered value is a window the server understands, and every one
    // round-trips to itself — `48h` would parse but canonicalise to `2d`, so
    // offering it would put a value in the list the server never echoes back.
    for v in &values {
        let window = tab_atelier_proxy::usage::Window::parse(v)
            .unwrap_or_else(|| panic!("index.html offers window {v:?}, which the server cannot parse"));
        assert_eq!(
            &window.token(),
            v,
            "index.html offers {v:?}, which the server canonicalises to {:?} — \
             the dropdown would snap to a different entry on the first refresh",
            window.token()
        );
    }

    // And the other direction: a token in `Window::TOKENS` that the dropdown
    // does not offer is a window nobody can select.
    for token in tab_atelier_proxy::usage::Window::TOKENS {
        assert!(
            values.iter().any(|v| v == token),
            "`Window::TOKENS` has {token:?} but index.html does not offer it"
        );
    }
}

/// The `value="…"` of every `<option>` inside the `<select>` bound to `model`.
fn select_values(html: &str, model: &str) -> Vec<String> {
    let bind = format!("v-model=\"{model}\"");
    let after = html.split_once(&bind).map_or("", |(_, rest)| rest);
    let block = after.split_once("</select>").map_or("", |(body, _)| body);
    block
        .split("<option")
        .skip(1)
        .filter_map(|o| o.split_once("value=\""))
        .filter_map(|(_, rest)| rest.split_once('"'))
        .map(|(v, _)| v.to_owned())
        .collect()
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}
