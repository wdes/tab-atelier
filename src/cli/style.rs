// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier style` — per-project (and per-tab) colour + badge.
//!
//! A folder rule gives every tab whose cwd is inside a project the same tint
//! and badge. Since a new tab inherits the active tab's cwd, that's also what
//! makes the identity survive Ctrl+Shift+T: nothing is copied from tab to tab,
//! it's re-derived from the folder.
//!
//! Folder rules live in `preferences.json` (written here, re-read by the
//! daemon on the next tick when the file's mtime moves). Per-tab overrides go
//! through the running daemon and win over the folder rule.

use super::share_link::{agent, discover_endpoint, resolve};

fn usage() {
    eprintln!(
        "usage:\n  \
         tab-atelier style --folder <dir> [--color #RRGGBB] [--badge TXT]\n  \
         tab-atelier style --folder <dir> --clear\n  \
         tab-atelier style --tab <idx-or-uuid> [--color #RRGGBB|clear] [--badge TXT|clear]\n  \
         tab-atelier style --list\n\n\
         A folder rule styles every tab whose cwd is inside it (longest match wins),\n\
         so tabs opened in a project — including with Ctrl+Shift+T — pick it up.\n\
         A per-tab override wins over the folder rule."
    );
}

/// Path used for matching: absolute, `~` expanded, trailing slash trimmed.
fn normalize_dir(dir: &str) -> Result<String, String> {
    let expanded = match dir.strip_prefix("~/") {
        Some(rest) => {
            std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "$HOME is unset".to_string())?).join(rest)
        }
        None => std::path::PathBuf::from(dir),
    };
    // Canonicalise when the directory exists (resolves `.`/`..`/symlinks); a
    // rule for a path that doesn't exist yet is still allowed, verbatim.
    let path = std::fs::canonicalize(&expanded).unwrap_or(expanded);
    let s = path.to_string_lossy().trim_end_matches('/').to_string();
    if s.starts_with('/') {
        Ok(s)
    } else {
        Err(format!("{dir}: expected an absolute path"))
    }
}

#[must_use]
#[allow(clippy::too_many_lines)] // one flat arg parse + three short branches
pub fn run(args: &[String]) -> i32 {
    let (mut folder, mut tab, mut color, mut badge) = (None, None, None, None);
    let (mut clear, mut list) = (false, false);
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let slot = match flag {
            "--folder" | "-f" => Some(&mut folder),
            "--tab" | "-t" => Some(&mut tab),
            "--color" | "-c" => Some(&mut color),
            "--badge" | "-b" => Some(&mut badge),
            "--clear" => {
                clear = true;
                None
            }
            "--list" | "-l" => {
                list = true;
                None
            }
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other => {
                eprintln!("style: unknown argument: {other}");
                return 2;
            }
        };
        if let Some(slot) = slot {
            i += 1;
            let Some(v) = args.get(i) else {
                eprintln!("style: {flag} expects a value");
                return 2;
            };
            *slot = Some(v.clone());
        }
        i += 1;
    }

    if let Some(ref c) = color
        && !c.eq_ignore_ascii_case("clear")
        && crate::parse_hex_rgb(c).is_none()
    {
        eprintln!("style: {c:?} is not #RRGGBB (or `clear`)");
        return 2;
    }
    if let Some(ref b) = badge
        && !b.eq_ignore_ascii_case("clear")
        && let Err(e) = crate::sanitize_badge(b)
    {
        eprintln!("style: {e}");
        return 2;
    }

    match (list, folder, tab) {
        (true, _, _) => list_rules(),
        (_, Some(dir), None) => set_folder(&dir, color.as_deref(), badge.as_deref(), clear),
        (_, None, Some(key)) => set_tab(&key, color.as_deref(), badge.as_deref(), clear),
        (_, Some(_), Some(_)) => {
            eprintln!("style: pass --folder or --tab, not both");
            2
        }
        (_, None, None) => {
            usage();
            2
        }
    }
}

/// Rules are read back as raw JSON so unrelated preference keys round-trip
/// untouched when we write the file again.
fn load_rules() -> std::collections::BTreeMap<String, crate::FolderStyle> {
    std::fs::read_to_string(crate::editable_preferences_path())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| serde_json::from_value(v.get("folder_styles")?.clone()).ok())
        .unwrap_or_default()
}

fn list_rules() -> i32 {
    let rules = load_rules();
    if rules.is_empty() {
        println!("no folder styles set — try: tab-atelier style --folder <dir> --color '#7a1f2b' --badge TAG");
        return 0;
    }
    for (dir, style) in &rules {
        println!(
            "{dir}  color={}  badge={}",
            style.color.as_deref().unwrap_or("-"),
            style.badge.as_deref().unwrap_or("-"),
        );
    }
    0
}

fn set_folder(dir: &str, color: Option<&str>, badge: Option<&str>, clear: bool) -> i32 {
    let dir = match normalize_dir(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("style: {e}");
            return 2;
        }
    };
    if !clear && color.is_none() && badge.is_none() {
        eprintln!("style: nothing to set — pass --color and/or --badge, or --clear");
        return 2;
    }
    let mut rules = load_rules();
    if clear {
        rules.remove(&dir);
    } else {
        let entry = rules.entry(dir.clone()).or_default();
        if let Some(c) = color {
            entry.color = (!c.eq_ignore_ascii_case("clear")).then(|| c.to_string());
        }
        if let Some(b) = badge {
            entry.badge = (!b.eq_ignore_ascii_case("clear")).then(|| b.to_string());
        }
        if entry.color.is_none() && entry.badge.is_none() {
            rules.remove(&dir);
        }
    }

    let path = crate::editable_preferences_path();
    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = doc.as_object_mut() else {
        eprintln!("style: preferences.json root is not an object");
        return 1;
    };
    if rules.is_empty() {
        obj.remove("folder_styles");
    } else {
        match serde_json::to_value(&rules) {
            Ok(v) => {
                obj.insert("folder_styles".into(), v);
            }
            Err(e) => {
                eprintln!("style: serialize: {e}");
                return 1;
            }
        }
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        eprintln!("style: write {}: {e}", path.display());
        return 1;
    }
    println!("updated {} (applies within a tick)", path.display());
    0
}

fn set_tab(key: &str, color: Option<&str>, badge: Option<&str>, clear: bool) -> i32 {
    if !clear && color.is_none() && badge.is_none() {
        eprintln!("style: nothing to set — pass --color and/or --badge, or --clear");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("style: {e}");
            return 1;
        }
    };
    let (_, uuid) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("style: {e}");
            return 1;
        }
    };
    // `--clear` drops both overrides, so the tab falls back to its folder rule.
    let color = if clear { Some("clear") } else { color };
    let badge = if clear { Some("clear") } else { badge };
    let post = |route: &str, body: String| -> Result<(), String> {
        agent()
            .post(format!("{}/tabs/by-id/{uuid}/{route}", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
            .map(|_| ())
            .map_err(|e| format!("{route}: {e}"))
    };
    let as_json = |v: &str| {
        if v.eq_ignore_ascii_case("clear") {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(v.to_string())
        }
    };
    if let Some(c) = color
        && let Err(e) = post("bg-color", serde_json::json!({ "color": as_json(c) }).to_string())
    {
        eprintln!("style: {e}");
        return 1;
    }
    if let Some(b) = badge
        && let Err(e) = post("badge", serde_json::json!({ "badge": as_json(b) }).to_string())
    {
        eprintln!("style: {e}");
        return 1;
    }
    println!("✓ tab {uuid} restyled");
    0
}
