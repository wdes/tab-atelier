// SPDX-License-Identifier: MPL-2.0

//! The committed UI bundle must match the TypeScript it came from.
//!
//! `assets/*.js` is generated from `web/src/*.ts` and committed, so that
//! building a `.deb` never needs Node. The cost of that choice is exactly one
//! failure mode — someone edits the `.ts` and forgets to rebuild, or edits the
//! generated `.js` directly and has it overwritten later — and this is what
//! closes it.

use std::collections::BTreeSet;
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

    // `api.js` is listed because it is served to the browser and must match its
    // source like the other two. A stale copy would be a client calling routes
    // the server no longer has — which is the failure this test exists for,
    // and the one hardest to see, since the page would still load.
    for name in ["api.js", "app.js", "charts.js"] {
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

/// Every function the template calls must be declared under `methods`.
///
/// A Vue `computed` is a getter: it is read as a property, so a template that
/// *calls* one — `priceLabel(m)` where `priceLabel` was declared under
/// `computed` — evaluates `undefined(m)` and throws. Nothing catches that
/// earlier. `tsc` types each entry by its return value and never looks at the
/// markup, so the sources compile and type-check cleanly; and Vue answers a
/// render error in the root component by mounting nothing. The page goes blank,
/// with one console line and no other symptom.
///
/// That is not hypothetical: it shipped in the money-unit change and the
/// dashboard was white until someone thought to open the console. This is the
/// guard for it.
///
/// Read out of the committed markup *and* the TypeScript, because the failure
/// spans both and neither is compiled with the other — `assets/index.html` is
/// where the call is, `web/src/app.ts` is where the answer has to be.
///
/// No `computed` here returns a function, which is what lets this be a plain
/// set-membership check. If one ever legitimately does, this test is where that
/// shows up, and `methods` is the clearer home for a callable anyway.
#[test]
fn every_function_the_template_calls_is_a_method() {
    let html = std::fs::read_to_string(crate_dir().join("assets/index.html")).expect("index.html");
    let ts = std::fs::read_to_string(crate_dir().join("web/src/app.ts")).expect("app.ts");

    let (computed, methods) = vue_entries(&ts);
    assert!(
        methods.len() > 20 && !computed.is_empty(),
        "parsed {} methods and {} computeds out of app.ts — the scan is broken, \
         and a broken scan passes everything",
        methods.len(),
        computed.len()
    );

    let called = calls_in_markup(&without_style(&html));
    assert!(!called.is_empty(), "no calls found in index.html — the scan is broken");

    for name in &called {
        assert!(
            !computed.contains(name),
            "index.html calls {name}(…), but {name} is declared under `computed`. \
             A computed is a getter, so this is `undefined(…)` at render time and the \
             dashboard goes blank. Move it to `methods`."
        );
        assert!(
            matches!(
                name.as_str(),
                "Math" | "Number" | "String" | "JSON" | "Object" | "Array" | "Date"
            ) || methods.contains(name),
            "index.html calls {name}(…), which is not declared under `methods` in app.ts. \
             It will be `undefined` at render time."
        );
    }
}

/// The names declared under `computed:` and `methods:` in the component.
///
/// Indentation-aware rather than a brace-follower: entries sit at four spaces
/// inside a section that opens and closes at two, and nothing nested ever does,
/// so indentation is both the simplest and the true reading of this file.
fn vue_entries(ts: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut computed = BTreeSet::new();
    let mut methods = BTreeSet::new();
    let mut in_methods: Option<bool> = None;

    for line in ts.lines() {
        if line == "  computed: {" {
            in_methods = Some(false);
            continue;
        }
        if line == "  methods: {" {
            in_methods = Some(true);
            continue;
        }
        if line == "  }," {
            in_methods = None;
            continue;
        }
        let Some(is_a_method) = in_methods else { continue };
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if rest.starts_with(' ') {
            continue;
        }
        // `async` comes before the name, so reading the first identifier
        // blindly files every async method under `async` — which is how the
        // first version of this check reported `addPreset`, a method that had
        // been in `methods` all along.
        let decl = rest.strip_prefix("async ").unwrap_or(rest);
        let name: String = decl
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if name.is_empty() {
            continue;
        }
        // An entry starts its declaration — `name(…)` or `name:` — where a
        // continuation line would start with punctuation instead. Measured
        // against `decl`, since `name` was read from there.
        let tail = &decl[name.len()..];
        let is_entry = tail.starts_with('(') || tail.starts_with(':') || tail.starts_with('<');
        if is_entry {
            if is_a_method {
                methods.insert(name);
            } else {
                computed.insert(name);
            }
        }
    }
    (computed, methods)
}

/// Operators and keywords that can precede `(` in a template expression.
///
/// Whitespace is allowed before a call here, so that `pad (n)` is still caught.
/// That tolerance is what makes `v-for="s in (…)"` read as a call to `in`, which
/// it is not — hence this list rather than requiring `name(` to be adjacent.
const KEYWORDS: [&str; 8] = ["in", "of", "typeof", "instanceof", "new", "void", "delete", "return"];

/// Every `name(` the template actually evaluates.
///
/// Both places its JavaScript lives: `{{ … }}` interpolations, and attribute
/// values — which covers `:bound="…"`, `@click="…"` and `v-if="…"` alike.
/// Member calls (`Math.max(…)`, `this.pad(…)`) are skipped: they resolve
/// through their receiver, which is `tsc`'s business, not this test's.
fn calls_in_markup(markup: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut fragments: Vec<&str> = Vec::new();

    let mut rest = markup;
    while let Some((_, after)) = rest.split_once("{{") {
        match after.split_once("}}") {
            Some((body, tail)) => {
                fragments.push(body);
                rest = tail;
            }
            None => break,
        }
    }

    let mut rest = markup;
    while let Some((_, after)) = rest.split_once("=\"") {
        match after.split_once('"') {
            Some((value, tail)) => {
                fragments.push(value);
                rest = tail;
            }
            None => break,
        }
    }

    for fragment in fragments {
        let bytes = fragment.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                    i += 1;
                }
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let after_dot = start > 0 && bytes[start - 1] == b'.';
                let name = &fragment[start..i];
                if !after_dot && !KEYWORDS.contains(&name) && j < bytes.len() && bytes[j] == b'(' {
                    found.insert(name.to_owned());
                }
            } else {
                i += 1;
            }
        }
    }
    found
}

/// `html` with its `<style>` block removed.
///
/// The stylesheet is full of `rgb(…)`, `gradient(…)` and `where(…)`, each of
/// which reads exactly like a call to the scan above.
fn without_style(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some((before, after)) = rest.split_once("<style") {
        out.push_str(before);
        match after.split_once("</style>") {
            Some((_, tail)) => rest = tail,
            // Unterminated: treat the remainder as stylesheet, since that is
            // what it is, and let the other tests complain about the markup.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}
