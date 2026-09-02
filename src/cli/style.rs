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

/// What one `style` invocation asks for, parsed off the command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StyleArgs {
    pub folder: Option<String>,
    pub tab: Option<String>,
    pub color: Option<String>,
    pub badge: Option<String>,
    pub clear: bool,
    pub list: bool,
}

/// Pure arg parser + validation, so the branch table is testable without a
/// preferences file or a running daemon.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on a missing value, an
/// unknown flag, a colour that isn't `#RRGGBB`, or a badge that fails
/// [`crate::sanitize_badge`].
pub fn parse_style_args(args: &[String]) -> Result<StyleArgs, i32> {
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
                return Err(0);
            }
            other => {
                eprintln!("style: unknown argument: {other}");
                return Err(2);
            }
        };
        if let Some(slot) = slot {
            i += 1;
            let Some(v) = args.get(i) else {
                eprintln!("style: {flag} expects a value");
                return Err(2);
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
        return Err(2);
    }
    if let Some(ref b) = badge
        && !b.eq_ignore_ascii_case("clear")
        && let Err(e) = crate::sanitize_badge(b)
    {
        eprintln!("style: {e}");
        return Err(2);
    }
    if folder.is_some() && tab.is_some() {
        eprintln!("style: pass --folder or --tab, not both");
        return Err(2);
    }
    if !list && folder.is_none() && tab.is_none() {
        usage();
        return Err(2);
    }
    Ok(StyleArgs {
        folder,
        tab,
        color,
        badge,
        clear,
        list,
    })
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_style_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let path = crate::editable_preferences_path();
    if parsed.list {
        return list_rules(&path);
    }
    if let Some(dir) = parsed.folder {
        return set_folder(
            &path,
            &dir,
            parsed.color.as_deref(),
            parsed.badge.as_deref(),
            parsed.clear,
        );
    }
    let Some(key) = parsed.tab else { return 2 };
    set_tab(&key, parsed.color.as_deref(), parsed.badge.as_deref(), parsed.clear)
}

/// Rules are read back as raw JSON so unrelated preference keys round-trip
/// untouched when we write the file again.
fn load_rules(path: &std::path::Path) -> std::collections::BTreeMap<String, crate::FolderStyle> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| serde_json::from_value(v.get("folder_styles")?.clone()).ok())
        .unwrap_or_default()
}

fn list_rules(path: &std::path::Path) -> i32 {
    let rules = load_rules(path);
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

fn set_folder(path: &std::path::Path, dir: &str, color: Option<&str>, badge: Option<&str>, clear: bool) -> i32 {
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
    let mut rules = load_rules(path);
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

    let mut doc: serde_json::Value = std::fs::read_to_string(path)
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
    if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_every_flag_and_its_short_form() {
        let a =
            parse_style_args(&args(&["--folder", "/tmp/p", "--color", "#7a1f2b", "--badge", "KAL"])).expect("valid");
        assert_eq!(a.folder.as_deref(), Some("/tmp/p"));
        assert_eq!(a.color.as_deref(), Some("#7a1f2b"));
        assert_eq!(a.badge.as_deref(), Some("KAL"));
        assert!(!a.clear && !a.list);
        let short = parse_style_args(&args(&["-f", "/tmp/p", "-c", "#000000", "-b", "K"])).expect("valid");
        assert_eq!(
            (short.folder.as_deref(), short.color.as_deref(), short.badge.as_deref()),
            (Some("/tmp/p"), Some("#000000"), Some("K"))
        );
        assert!(parse_style_args(&args(&["--list"])).expect("valid").list);
        let cleared = parse_style_args(&args(&["--tab", "3", "--clear"])).expect("valid");
        assert!(cleared.clear && cleared.tab.as_deref() == Some("3"));
    }

    #[test]
    fn rejects_bad_input_before_touching_anything() {
        // `clear` is the one non-hex colour word accepted, so a typo can't be
        // silently written into preferences.json.
        assert_eq!(parse_style_args(&args(&["-f", "/tmp/p", "-c", "red"])), Err(2));
        assert!(parse_style_args(&args(&["-f", "/tmp/p", "-c", "clear"])).is_ok());
        assert!(parse_style_args(&args(&["-f", "/tmp/p", "-c", "CLEAR"])).is_ok());
        assert_eq!(parse_style_args(&args(&["-f", "/tmp/p", "-b", "TOOLONGBADGE"])), Err(2));
        assert_eq!(parse_style_args(&args(&["--folder"])), Err(2), "flag with no value");
        assert_eq!(parse_style_args(&args(&["--nope"])), Err(2));
        // A folder rule and a tab override are different targets.
        assert_eq!(
            parse_style_args(&args(&["-f", "/tmp/p", "-t", "3", "-c", "#000000"])),
            Err(2)
        );
        // Nothing to do at all is usage, not a silent success.
        assert_eq!(parse_style_args(&args(&[])), Err(2));
        assert_eq!(parse_style_args(&args(&["--help"])), Err(0));
    }

    #[test]
    fn normalize_dir_makes_a_matchable_absolute_path() {
        // Rules match by cwd prefix, so a stored path must be absolute and
        // free of a trailing slash or nothing would ever match it.
        assert_eq!(normalize_dir("/tmp/").as_deref(), Ok("/tmp"));
        assert_eq!(
            normalize_dir("/nope/does-not-exist/").as_deref(),
            Ok("/nope/does-not-exist")
        );
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(normalize_dir("~/x").as_deref(), Ok(format!("{home}/x").as_str()));
        assert!(normalize_dir("relative/path").is_err());
    }

    /// A preferences file with an unrelated key, to prove we never eat it.
    fn seed_prefs(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("preferences.json");
        std::fs::write(&p, r#"{"theme":"tomorrow-night-blue","opacity":95}"#).expect("seed");
        p
    }

    #[test]
    fn folder_rules_round_trip_without_disturbing_other_preferences() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = seed_prefs(tmp.path());
        assert_eq!(set_folder(&path, "/tmp", Some("#123456"), Some("T"), false), 0);
        let rules = load_rules(&path);
        assert_eq!(rules["/tmp"].color.as_deref(), Some("#123456"));
        assert_eq!(rules["/tmp"].badge.as_deref(), Some("T"));
        // Editing one field leaves the other alone.
        assert_eq!(set_folder(&path, "/tmp", None, Some("U"), false), 0);
        let rules = load_rules(&path);
        assert_eq!(rules["/tmp"].color.as_deref(), Some("#123456"), "colour survived");
        assert_eq!(rules["/tmp"].badge.as_deref(), Some("U"));
        // Unrelated preferences round-trip untouched — the whole reason this
        // patches raw JSON instead of serialising a Preferences struct.
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(doc["theme"], "tomorrow-night-blue");
        assert_eq!(doc["opacity"], 95);
    }

    #[test]
    fn clearing_the_last_field_drops_the_rule_and_then_the_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = seed_prefs(tmp.path());
        assert_eq!(set_folder(&path, "/tmp", Some("#123456"), Some("T"), false), 0);
        // A rule with neither colour nor badge is not a rule.
        assert_eq!(set_folder(&path, "/tmp", Some("clear"), Some("clear"), false), 0);
        assert!(load_rules(&path).is_empty());
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert!(doc.get("folder_styles").is_none(), "empty map removes the key");
        // …and an explicit --clear removes just that folder.
        assert_eq!(set_folder(&path, "/tmp", Some("#123456"), None, false), 0);
        assert_eq!(set_folder(&path, "/usr", Some("#654321"), None, false), 0);
        assert_eq!(set_folder(&path, "/tmp", None, None, true), 0);
        let rules = load_rules(&path);
        assert!(!rules.contains_key("/tmp") && rules.contains_key("/usr"));
    }

    #[test]
    fn set_folder_refuses_a_write_with_nothing_to_write() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = seed_prefs(tmp.path());
        assert_eq!(set_folder(&path, "/tmp", None, None, false), 2);
        assert!(load_rules(&path).is_empty());
        // A missing file is not an error — the first rule creates it.
        let fresh = tmp.path().join("new.json");
        assert_eq!(set_folder(&fresh, "/tmp", Some("#abcdef"), None, false), 0);
        assert_eq!(load_rules(&fresh)["/tmp"].color.as_deref(), Some("#abcdef"));
    }

    #[test]
    fn listing_is_read_only_and_survives_a_missing_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(list_rules(&tmp.path().join("absent.json")), 0);
        let path = seed_prefs(tmp.path());
        assert_eq!(set_folder(&path, "/tmp", Some("#123456"), None, false), 0);
        assert_eq!(list_rules(&path), 0);
        // Garbage in the file must not panic the CLI, just yield no rules.
        std::fs::write(&path, "not json at all").expect("write");
        assert!(load_rules(&path).is_empty());
        assert_eq!(list_rules(&path), 0);
    }
}
