// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_DIR: &str = "tab-atelier";

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub name: String,
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_wh: Option<f64>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedState {
    pub tabs: Vec<TabState>,
    pub active: usize,
}

#[must_use]
pub fn state_dir(base: &std::path::Path) -> PathBuf {
    base.join(APP_DIR)
}

#[must_use]
pub fn state_path(base: &std::path::Path) -> PathBuf {
    state_dir(base).join("tabs.json")
}

#[must_use]
pub fn load_state_from(base: &std::path::Path) -> Option<SavedState> {
    let path = state_path(base);
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

#[must_use]
pub fn load_font_config(config_base: &std::path::Path) -> FontConfig {
    let config_path = config_base.join("zed/settings.json");
    load_font_config_from(&config_path)
}

#[must_use]
pub fn load_font_config_from(path: &std::path::Path) -> FontConfig {
    let mut config = FontConfig::default();

    let Ok(data) = std::fs::read_to_string(path) else {
        return config;
    };

    let stripped: String = strip_json_comments(&data);

    let Ok(parsed): Result<serde_json::Value, _> = serde_json::from_str(&stripped) else {
        return config;
    };

    if let Some(family) = parsed.get("ui_font_family").and_then(|v| v.as_str()) {
        config.family = family.to_string();
    }
    if let Some(weight) = parsed.get("ui_font_weight").and_then(serde_json::Value::as_u64) {
        config.weight = weight as u16;
    }
    if let Some(size) = parsed.get("ui_font_size").and_then(serde_json::Value::as_f64) {
        config.size = size as f32;
    } else if let Some(size) = parsed.get("buffer_font_size").and_then(serde_json::Value::as_f64) {
        config.size = size as f32;
    }
    if let Some(sens) = parsed.get("scroll_sensitivity").and_then(serde_json::Value::as_f64) {
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

#[must_use]
pub fn load_wakatime_key(config_base: &std::path::Path) -> Option<String> {
    let config_path = config_base.join("zed/settings.json");
    let data = std::fs::read_to_string(config_path).ok()?;
    let stripped = strip_json_comments(&data);
    let parsed: serde_json::Value = serde_json::from_str(&stripped).ok()?;
    parsed
        .get("wakatime")
        .and_then(|w| w.get("settings"))
        .and_then(|s| s.get("api-key"))
        .and_then(|k| k.as_str())
        .map(std::string::ToString::to_string)
}

#[derive(Serialize, Deserialize, Default)]
pub struct Preferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_editor: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub id: &'static str,
    pub label: &'static str,
    pub keycode: u8,
}

pub static HOTKEY_OPTIONS: &[Hotkey] = &[
    Hotkey {
        id: "grave",
        label: "` (Grave)",
        keycode: 49,
    },
    Hotkey {
        id: "f12",
        label: "F12",
        keycode: 96,
    },
    Hotkey {
        id: "f11",
        label: "F11",
        keycode: 95,
    },
    Hotkey {
        id: "f1",
        label: "F1",
        keycode: 67,
    },
    Hotkey {
        id: "xf86calculator",
        label: "XF86Calculator",
        keycode: 148,
    },
];

pub static DEFAULT_HOTKEYS: &[&str] = &["grave", "xf86calculator"];

#[must_use]
pub fn hotkey_keycodes(ids: &[String]) -> Vec<u8> {
    ids.iter()
        .filter_map(|id| HOTKEY_OPTIONS.iter().find(|h| h.id == id).map(|h| h.keycode))
        .collect()
}

#[must_use]
pub fn load_preferences(base: &std::path::Path) -> Preferences {
    let path = state_dir(base).join("preferences.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

pub fn save_preferences(base: &std::path::Path, prefs: &Preferences) {
    let dir = state_dir(base);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("preferences.json");
    if let Ok(data) = serde_json::to_string_pretty(prefs) {
        let _ = std::fs::write(path, data);
    }
}

pub fn save_state(base: &std::path::Path, state: &SavedState) {
    let dir = state_dir(base);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tabs.json");
    if let Ok(data) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, data);
    }
}

#[must_use]
pub fn detect_urls(text: &str) -> Vec<(usize, usize, String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut urls = Vec::new();
    let mut i = 0;

    while i < len {
        if chars[i] == 'h' && i + 7 < len {
            let prefix_len = if i + 8 <= len
                && chars[i + 1] == 't'
                && chars[i + 2] == 't'
                && chars[i + 3] == 'p'
                && chars[i + 4] == 's'
                && chars[i + 5] == ':'
                && chars[i + 6] == '/'
                && chars[i + 7] == '/'
            {
                8
            } else if i + 7 <= len
                && chars[i + 1] == 't'
                && chars[i + 2] == 't'
                && chars[i + 3] == 'p'
                && chars[i + 4] == ':'
                && chars[i + 5] == '/'
                && chars[i + 6] == '/'
            {
                7
            } else {
                0
            };
            if prefix_len > 0 {
                let start = i;
                while i < len
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '"' | '\'' | '<' | '>' | ')' | ']' | '}')
                {
                    i += 1;
                }
                while i > start + prefix_len && matches!(chars[i - 1], '.' | ',' | ';') {
                    i -= 1;
                }
                let url: String = chars[start..i].iter().collect();
                urls.push((start, i, url, false));
                continue;
            }
        }

        if chars[i] == '/' && i + 1 < len && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '.') {
            let mut start = i;
            while start > 0 && (chars[start - 1].is_alphanumeric() || matches!(chars[start - 1], '_' | '-' | '.')) {
                start -= 1;
            }
            let mut j = i;
            while j < len
                && !chars[j].is_whitespace()
                && !matches!(chars[j], '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '|' | '│')
            {
                j += 1;
            }
            let path: String = chars[start..j].iter().collect();
            if path.matches('/').count() >= 2 {
                urls.push((start, j, path, true));
                i = j;
                continue;
            }
        }

        if i + 4 < len && chars[i].is_alphanumeric() {
            let start = i;
            let mut j = i;
            while j < len && !chars[j].is_whitespace() && !matches!(chars[j], '"' | '\'' | '<' | '>' | ')' | ']' | '}')
            {
                j += 1;
            }
            let candidate: String = chars[start..j].iter().collect();
            if candidate.contains('/') && candidate.contains(':') {
                let has_slash = candidate.matches('/').count() >= 1;
                let colon_part = candidate.rsplit(':').next().unwrap_or("");
                let looks_like_path =
                    has_slash && !colon_part.is_empty() && colon_part.chars().all(|c| c.is_ascii_digit());
                if looks_like_path && !candidate.starts_with("http") {
                    urls.push((start, j, candidate, true));
                    i = j;
                    continue;
                }
            }
        }

        i += 1;
    }

    urls
}

#[must_use]
pub fn file_path_for_open(path: &str) -> &str {
    if let Some(colon_pos) = path.rfind(':') {
        let after = &path[colon_pos + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            let base = &path[..colon_pos];
            if let Some(colon_pos2) = base.rfind(':') {
                let after2 = &base[colon_pos2 + 1..];
                if !after2.is_empty() && after2.chars().all(|c| c.is_ascii_digit()) {
                    return &path[..colon_pos2];
                }
            }
            return base;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tab_state_serialization() {
        let state = SavedState {
            tabs: vec![
                TabState {
                    name: "Terminal".into(),
                    cwd: Some("/home/user".into()),
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                },
                TabState {
                    name: "Build".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                },
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
    fn test_tab_state_uptime_energy_round_trip() {
        let state = SavedState {
            tabs: vec![TabState {
                name: "T".into(),
                cwd: None,
                output: None,
                uptime_secs: Some(123.5),
                energy_wh: Some(0.042),
            }],
            active: 0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!((restored.tabs[0].uptime_secs.unwrap() - 123.5).abs() < f64::EPSILON);
        assert!((restored.tabs[0].energy_wh.unwrap() - 0.042).abs() < f64::EPSILON);
    }

    #[test]
    fn test_tab_state_uptime_energy_defaults() {
        let json = r#"{"tabs":[{"name":"X","cwd":null}],"active":0}"#;
        let restored: SavedState = serde_json::from_str(json).unwrap();
        assert!(restored.tabs[0].uptime_secs.is_none());
        assert!(restored.tabs[0].energy_wh.is_none());
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
    fn test_state_path_uses_base() {
        let path = state_path(std::path::Path::new("/tmp/test-base"));
        assert!(path.ends_with(format!("{APP_DIR}/tabs.json")));
    }

    #[test]
    fn test_load_state_missing_file() {
        let result = load_state_from(std::path::Path::new("/tmp/ta-test-nonexistent"));
        assert!(result.is_none());
    }

    #[test]
    fn test_save_then_load_round_trip() {
        let dir = std::env::temp_dir().join("ta-test-round-trip");
        let _ = std::fs::create_dir_all(&dir);

        let state = SavedState {
            tabs: vec![
                TabState {
                    name: "One".into(),
                    cwd: Some("/tmp".into()),
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                },
                TabState {
                    name: "Two".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                },
            ],
            active: 1,
        };
        save_state(&dir, &state);
        let loaded = load_state_from(&dir).expect("should load saved state");
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.tabs[0].name, "One");
        assert_eq!(loaded.tabs[0].cwd, Some("/tmp".into()));
        assert_eq!(loaded.tabs[1].name, "Two");
        assert_eq!(loaded.tabs[1].cwd, None);
        assert_eq!(loaded.active, 1);

        let _ = std::fs::remove_dir_all(dir.join(APP_DIR));
    }

    #[test]
    fn test_load_state_malformed_json() {
        let dir = std::env::temp_dir().join("ta-test-malformed");
        let sd = dir.join(APP_DIR);
        let _ = std::fs::create_dir_all(&sd);
        std::fs::write(sd.join("tabs.json"), "not json").unwrap();

        let result = load_state_from(&dir);
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&sd);
    }

    #[test]
    fn test_state_dir_has_app_dir() {
        let dir = state_dir(std::path::Path::new("/tmp/test"));
        assert_eq!(dir.file_name().unwrap(), APP_DIR);
    }

    #[test]
    fn test_state_dir_with_base() {
        let dir = state_dir(std::path::Path::new("/tmp/custom-state"));
        assert_eq!(dir, PathBuf::from(format!("/tmp/custom-state/{APP_DIR}")));
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
        let fc = load_font_config_from(std::path::Path::new("/tmp/nonexistent-config.json"));
        assert_eq!(fc.family, "monospace");
        assert_eq!(fc.weight, 400);
    }

    #[test]
    fn test_load_font_config_partial() {
        let dir = std::env::temp_dir().join("ta-test-font");
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
        let dir = std::env::temp_dir().join("ta-test-font-fallback");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "buffer_font_size": 20 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.size - 20.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity() {
        let dir = std::env::temp_dir().join("ta-test-scroll-sens");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "scroll_sensitivity": 2.5 }"#).unwrap();
        let fc = load_font_config_from(&path);
        assert!((fc.scroll_sensitivity - 2.5).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_font_config_scroll_sensitivity_clamped() {
        let dir = std::env::temp_dir().join("ta-test-scroll-clamp");
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
        let dir = std::env::temp_dir().join("ta-test-comments");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
  // font settings
  "ui_font_family": "Fira Code",
  "ui_font_weight": 700,
  "ui_font_size": 18
}"#,
        )
        .unwrap();
        let fc = load_font_config_from(&path);
        assert_eq!(fc.family, "Fira Code");
        assert_eq!(fc.weight, 700);
        assert!((fc.size - 18.0).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_state_creates_directory() {
        let dir = std::env::temp_dir().join("ta-test-create-dir");
        let _ = std::fs::remove_dir_all(&dir);
        let state = SavedState {
            tabs: vec![TabState {
                name: "T".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
            }],
            active: 0,
        };
        save_state(&dir, &state);
        assert!(dir.join(format!("{APP_DIR}/tabs.json")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_http_url() {
        let urls = detect_urls("visit https://example.com/page today");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "https://example.com/page");
        assert!(!urls[0].3);
    }

    #[test]
    fn detect_http_url_with_query() {
        let urls = detect_urls("go to http://localhost:3000/api?key=val&x=1");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "http://localhost:3000/api?key=val&x=1");
    }

    #[test]
    fn detect_url_trims_trailing_punctuation() {
        let urls = detect_urls("see https://example.com.");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "https://example.com");
    }

    #[test]
    fn detect_file_path() {
        let urls = detect_urls("error at /home/user/src/main.rs:42:5");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/home/user/src/main.rs:42:5");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_file_path_needs_two_components() {
        let urls = detect_urls("see /tmp or /dev");
        assert!(urls.is_empty());
    }

    #[test]
    fn detect_multiple_urls() {
        let urls = detect_urls("https://a.com and /home/user/file.rs");
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn file_path_strip_line_col() {
        assert_eq!(file_path_for_open("/src/main.rs:42:5"), "/src/main.rs");
        assert_eq!(file_path_for_open("/src/main.rs:42"), "/src/main.rs");
        assert_eq!(file_path_for_open("/src/main.rs"), "/src/main.rs");
    }

    #[test]
    fn no_urls_in_plain_text() {
        let urls = detect_urls("hello world nothing here");
        assert!(urls.is_empty());
    }

    #[test]
    fn detect_partial_path_with_line() {
        let urls = detect_urls("error at src/main.php:42");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "src/main.php:42");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_partial_path_with_line_col() {
        let urls = detect_urls("see src/lib/utils.rs:10:5 for details");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "src/lib/utils.rs:10:5");
        assert!(urls[0].3);
    }

    #[test]
    fn detect_relative_path_with_prefix() {
        let urls = detect_urls("│ phpMyAdmin/2026/02/detailed-report.md |");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "phpMyAdmin/2026/02/detailed-report.md");
        assert!(urls[0].3);
    }

    #[test]
    fn file_path_for_open_partial() {
        assert_eq!(file_path_for_open("src/main.php:42"), "src/main.php");
        assert_eq!(file_path_for_open("src/lib/utils.rs:10:5"), "src/lib/utils.rs");
    }

    #[test]
    fn test_active_clamped_on_load() {
        let dir = std::env::temp_dir().join("ta-test-clamp-active");
        let sd = dir.join(APP_DIR);
        let _ = std::fs::create_dir_all(&sd);
        let state = SavedState {
            tabs: vec![TabState {
                name: "Only".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
            }],
            active: 999,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(sd.join("tabs.json"), json).unwrap();

        let loaded = load_state_from(&dir).unwrap();
        assert_eq!(loaded.active, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
