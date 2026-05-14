// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub name: String,
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedState {
    pub tabs: Vec<TabState>,
    pub active: usize,
}

pub fn state_path() -> PathBuf {
    let dir = dirs_or_default();
    dir.join("tabs.json")
}

pub fn dirs_or_default() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        });
    base.join("swoop")
}

pub fn load_state() -> Option<SavedState> {
    let path = state_path();
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

#[derive(Debug, Clone)]
pub struct FontConfig {
    pub family: String,
    pub weight: u16,
    pub size: f32,
    pub scroll_sensitivity: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            weight: 400,
            size: 16.0,
            scroll_sensitivity: 1.0,
        }
    }
}

pub fn load_font_config() -> FontConfig {
    let config_path = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        })
        .join("zed/settings.json");

    load_font_config_from(&config_path)
}

pub fn load_font_config_from(path: &std::path::Path) -> FontConfig {
    let mut config = FontConfig::default();

    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return config,
    };

    // Zed settings.json has comments — strip them before parsing
    let stripped: String = strip_json_comments(&data);

    let parsed: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(_) => return config,
    };

    if let Some(family) = parsed.get("ui_font_family").and_then(|v| v.as_str()) {
        config.family = family.to_string();
    }
    if let Some(weight) = parsed.get("ui_font_weight").and_then(|v| v.as_u64()) {
        config.weight = weight as u16;
    }
    if let Some(size) = parsed.get("ui_font_size").and_then(|v| v.as_f64()) {
        config.size = size as f32;
    } else if let Some(size) = parsed.get("buffer_font_size").and_then(|v| v.as_f64()) {
        config.size = size as f32;
    }
    if let Some(sens) = parsed.get("scroll_sensitivity").and_then(|v| v.as_f64()) {
        config.scroll_sensitivity = (sens as f32).max(0.01);
    }

    config
}

fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    out.push(next);
                    chars.next();
                }
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if ch == '/' {
            match chars.peek() {
                Some(&'/') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some(&'*') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn load_wakatime_key() -> Option<String> {
    let config_path = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        })
        .join("zed/settings.json");

    let data = std::fs::read_to_string(config_path).ok()?;
    let stripped = strip_json_comments(&data);
    let parsed: serde_json::Value = serde_json::from_str(&stripped).ok()?;
    parsed.get("wakatime")
        .and_then(|w| w.get("settings"))
        .and_then(|s| s.get("api-key"))
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
}

pub fn save_state(state: &SavedState) {
    let dir = dirs_or_default();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tabs.json");
    if let Ok(data) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn set_env(key: &str, val: &str) {
        unsafe { std::env::set_var(key, val) }
    }

    unsafe fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn test_tab_state_serialization() {
        let state = SavedState {
            tabs: vec![
                TabState { name: "Terminal".into(), cwd: Some("/home/user".into()), output: None },
                TabState { name: "Build".into(), cwd: None, output: None },
            ],
            active: 1,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.tabs[0].name, "Terminal");
        assert_eq!(restored.tabs[0].cwd, Some("/home/user".into()));
        assert_eq!(restored.tabs[1].name, "Build");
        assert_eq!(restored.tabs[1].cwd, None);
        assert_eq!(restored.active, 1);
    }

    #[test]
    fn test_tab_state_empty_tabs() {
        let state = SavedState {
            tabs: vec![],
            active: 0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!(restored.tabs.is_empty());
    }

    #[test]
    fn test_state_path_uses_xdg() {
        let path = state_path();
        assert!(path.ends_with("swoop/tabs.json"));
    }

    #[test]
    fn test_load_state_missing_file() {
        let prev = std::env::var("XDG_STATE_HOME").ok();
        unsafe { set_env("XDG_STATE_HOME", "/tmp/swoop-test-nonexistent") };
        let result = load_state();
        assert!(result.is_none());
        match prev {
            Some(v) => unsafe { set_env("XDG_STATE_HOME", &v) },
            None => unsafe { remove_env("XDG_STATE_HOME") },
        }
    }

    #[test]
    fn test_save_then_load_round_trip() {
        let dir = std::env::temp_dir().join("swoop-test-round-trip");
        let _ = std::fs::create_dir_all(&dir);
        unsafe { set_env("XDG_STATE_HOME", dir.to_str().unwrap()) };

        let state = SavedState {
            tabs: vec![
                TabState { name: "One".into(), cwd: Some("/tmp".into()), output: None },
                TabState { name: "Two".into(), cwd: None, output: None },
            ],
            active: 1,
        };
        save_state(&state);
        let loaded = load_state().expect("should load saved state");
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.tabs[0].name, "One");
        assert_eq!(loaded.tabs[0].cwd, Some("/tmp".into()));
        assert_eq!(loaded.tabs[1].name, "Two");
        assert_eq!(loaded.tabs[1].cwd, None);
        assert_eq!(loaded.active, 1);

        let _ = std::fs::remove_dir_all(dir.join("swoop"));
    }

    #[test]
    fn test_load_state_malformed_json() {
        let dir = std::env::temp_dir().join("swoop-test-malformed");
        let swoop_dir = dir.join("swoop");
        let _ = std::fs::create_dir_all(&swoop_dir);
        std::fs::write(swoop_dir.join("tabs.json"), "not json").unwrap();
        unsafe { set_env("XDG_STATE_HOME", dir.to_str().unwrap()) };

        let result = load_state();
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&swoop_dir);
    }

    #[test]
    fn test_dirs_or_default_has_swoop() {
        let dir = dirs_or_default();
        assert_eq!(dir.file_name().unwrap(), "swoop");
    }

    #[test]
    fn test_dirs_respects_xdg_state_home() {
        unsafe { set_env("XDG_STATE_HOME", "/tmp/custom-state") };
        let dir = dirs_or_default();
        assert_eq!(dir, PathBuf::from("/tmp/custom-state/swoop"));
        unsafe { remove_env("XDG_STATE_HOME") };
    }

    #[test]
    fn test_font_config_default() {
        let fc = FontConfig::default();
        assert_eq!(fc.family, "monospace");
        assert_eq!(fc.weight, 400);
        assert!((fc.size - 16.0).abs() < f32::EPSILON);
        assert!((fc.scroll_sensitivity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_load_font_config_missing_file() {
        let fc = load_font_config_from(std::path::Path::new("/tmp/nonexistent-swoop.json"));
        assert_eq!(fc.family, "monospace");
        assert_eq!(fc.weight, 400);
    }

    #[test]
    fn test_load_font_config_partial() {
        let dir = std::env::temp_dir().join("swoop-test-font");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "ui_font_family": "JetBrains Mono", "ui_font_size": 14 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert_eq!(fc.family, "JetBrains Mono");
        assert!((fc.size - 14.0).abs() < f32::EPSILON);
        assert_eq!(fc.weight, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_buffer_font_fallback() {
        let dir = std::env::temp_dir().join("swoop-test-font-fallback");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "buffer_font_size": 20 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.size - 20.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity() {
        let dir = std::env::temp_dir().join("swoop-test-scroll-sens");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "scroll_sensitivity": 2.5 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.scroll_sensitivity - 2.5).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity_clamped() {
        let dir = std::env::temp_dir().join("swoop-test-scroll-clamp");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "scroll_sensitivity": 0.001 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.scroll_sensitivity - 0.01).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_strip_json_comments_line() {
        let input = r#"{
  // this is a comment
  "key": "value"
}"#;
        let out = strip_json_comments(input);
        assert!(!out.contains("comment"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn test_strip_json_comments_block() {
        let input = r#"{ /* block comment */ "a": 1 }"#;
        let out = strip_json_comments(input);
        assert!(!out.contains("block"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn test_strip_json_comments_preserves_strings() {
        let input = r#"{ "url": "https://example.com" }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://example.com");
    }

    #[test]
    fn test_strip_json_comments_slash_in_string() {
        let input = r#"{ "path": "a//b", "x": 1 }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["path"], "a//b");
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn test_strip_json_comments_escaped_quote() {
        let input = r#"{ "s": "he said \"hi\"", "n": 1 }"#;
        let out = strip_json_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["s"], r#"he said "hi""#);
    }

    #[test]
    fn test_load_font_config_with_comments() {
        let dir = std::env::temp_dir().join("swoop-test-comments");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{
  // font settings
  "ui_font_family": "Fira Code",
  "ui_font_weight": 700,
  "ui_font_size": 18
}"#).unwrap();
        let fc = load_font_config_from(&path);
        assert_eq!(fc.family, "Fira Code");
        assert_eq!(fc.weight, 700);
        assert!((fc.size - 18.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_state_creates_directory() {
        let dir = std::env::temp_dir().join("swoop-test-create-dir");
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { set_env("XDG_STATE_HOME", dir.to_str().unwrap()) };
        let state = SavedState {
            tabs: vec![TabState { name: "T".into(), cwd: None, output: None }],
            active: 0,
        };
        save_state(&state);
        assert!(dir.join("swoop/tabs.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_active_clamped_on_load() {
        let dir = std::env::temp_dir().join("swoop-test-clamp-active");
        let swoop_dir = dir.join("swoop");
        let _ = std::fs::create_dir_all(&swoop_dir);
        let state = SavedState {
            tabs: vec![TabState { name: "Only".into(), cwd: None, output: None }],
            active: 999,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(swoop_dir.join("tabs.json"), json).unwrap();
        unsafe { set_env("XDG_STATE_HOME", dir.to_str().unwrap()) };

        let loaded = load_state().unwrap();
        assert_eq!(loaded.active, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
