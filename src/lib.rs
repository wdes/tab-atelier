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
    /// Cumulative catbus-agent token usage for this tab. Both fields are
    /// zero when no agent session has run yet; skipped entirely in the
    /// serialized file when absent so the common (non-agent) case stays clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    /// `colors_enabled` for this tab — false means the shell was started
    /// with `TERM=dumb` (right-click → Disable colors). Skipped when
    /// `true` so the common case stays out of the serialized file.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub colors_enabled: bool,
}

const fn default_true() -> bool {
    true
}
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Serialize, Deserialize)]
pub struct SavedState {
    pub tabs: Vec<TabState>,
    pub active: usize,
    /// `true` when the user had toggled "Windowed mode" (Guake-style drop-down
    /// is the default, hence the field's name in the negative). Skipped when
    /// `false` so an unchanged session stays out of the serialized file.
    #[serde(default, skip_serializing_if = "is_false")]
    pub windowed: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

#[must_use]
pub fn state_dir(base: &std::path::Path) -> PathBuf {
    base.join(APP_DIR)
}

/// Sub-directory that holds the global tab list and preferences,
/// underneath `config_base_dir()` (e.g. `~/.local/tab-atelier`).
#[must_use]
pub fn config_dir(base: &std::path::Path) -> PathBuf {
    base.join(APP_DIR)
}

#[must_use]
pub fn state_path(base: &std::path::Path) -> PathBuf {
    state_dir(base).join("tabs.json")
}

#[must_use]
pub fn config_state_path(config_base: &std::path::Path) -> PathBuf {
    config_dir(config_base).join("tabs.json")
}

/// CRC32 (IEEE) — small inline implementation; used to disambiguate tab
/// names whose sanitized form would otherwise collide (e.g. `foo/bar` and
/// `foo_bar` both sanitize to `foo_bar`).
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    let mut crc: u32 = !0;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLY & mask);
        }
    }
    !crc
}

/// Sanitize a tab name into something safe to use as a filename component
/// and append a CRC32 of the original name so two tabs whose sanitized
/// forms collide still get distinct files.
///
/// Non-alphanumeric and non-`._-` characters become `_`. Result is bounded
/// in length so very long names don't blow past OS path limits.
#[must_use]
pub fn sanitize_tab_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    if out.len() > 100 {
        out.truncate(100);
    }
    let hash = crc32(name.as_bytes());
    format!("{out}-{hash:08x}")
}

#[must_use]
pub fn tab_output_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("output_tab-{}.json", sanitize_tab_filename(tab_name)))
}

#[must_use]
pub fn tab_power_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("power_tab-{}.json", sanitize_tab_filename(tab_name)))
}

#[must_use]
pub fn tab_uptime_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("uptime_tab-{}.json", sanitize_tab_filename(tab_name)))
}

pub fn save_tab_uptime(state_base: &std::path::Path, tab_name: &str, uptime_secs: f64) {
    let dir = state_dir(state_base);
    let path = tab_uptime_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &uptime_secs);
}

#[must_use]
pub fn load_tab_uptime(state_base: &std::path::Path, tab_name: &str) -> Option<f64> {
    load_f64_with_bak(&tab_uptime_path(state_base, tab_name))
}

pub fn save_tab_energy(state_base: &std::path::Path, tab_name: &str, energy_wh: f64) {
    let dir = state_dir(state_base);
    let path = tab_power_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &energy_wh);
}

#[must_use]
pub fn load_tab_energy(state_base: &std::path::Path, tab_name: &str) -> Option<f64> {
    load_f64_with_bak(&tab_power_path(state_base, tab_name))
}

/// Cumulative token usage for one tab's catbus-agent session.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

#[must_use]
pub fn tab_tokens_path(state_base: &std::path::Path, tab_name: &str) -> PathBuf {
    state_dir(state_base).join(format!("tokens_tab-{}.json", sanitize_tab_filename(tab_name)))
}

pub fn save_tab_tokens(state_base: &std::path::Path, tab_name: &str, usage: &TokenUsage) {
    let dir = state_dir(state_base);
    let path = tab_tokens_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, usage);
}

#[must_use]
pub fn load_tab_tokens(state_base: &std::path::Path, tab_name: &str) -> Option<TokenUsage> {
    let path = tab_tokens_path(state_base, tab_name);
    if let Ok(data) = std::fs::read_to_string(&path)
        && let Ok(v) = serde_json::from_str::<TokenUsage>(&data)
    {
        return Some(v);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(v) = serde_json::from_str::<TokenUsage>(&data)
    {
        return Some(v);
    }
    None
}

fn load_f64_with_bak(path: &std::path::Path) -> Option<f64> {
    if let Ok(data) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<f64>(&data)
    {
        return Some(v);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(v) = serde_json::from_str::<f64>(&data)
    {
        return Some(v);
    }
    None
}

#[must_use]
pub fn load_state_from(base: &std::path::Path) -> Option<SavedState> {
    load_state_at(&state_path(base))
}

#[must_use]
pub fn load_state_at(path: &std::path::Path) -> Option<SavedState> {
    if let Ok(data) = std::fs::read_to_string(path)
        && let Ok(state) = serde_json::from_str::<SavedState>(&data)
    {
        return Some(state);
    }
    // Primary file missing or corrupt — try rotated backups, newest first.
    for ext in ["bak", "bak.1", "bak.2"] {
        let alt = path.with_extension(format!("json.{ext}"));
        if let Ok(data) = std::fs::read_to_string(&alt)
            && let Ok(state) = serde_json::from_str::<SavedState>(&data)
        {
            log::warn!("loaded state from backup {}", alt.display());
            return Some(state);
        }
    }
    None
}

/// Load tab list and hydrate each tab's output / uptime / energy from its
/// per-tab file under `state_base`.
#[must_use]
pub fn load_state_with_outputs(config_base: &std::path::Path, state_base: &std::path::Path) -> Option<SavedState> {
    let mut state = load_state_at(&config_state_path(config_base))?;
    for t in &mut state.tabs {
        if t.output.is_none() {
            t.output = load_tab_output(state_base, &t.name);
        }
        if t.uptime_secs.is_none() {
            t.uptime_secs = load_tab_uptime(state_base, &t.name);
        }
        if t.energy_wh.is_none() {
            t.energy_wh = load_tab_energy(state_base, &t.name);
        }
        if t.tokens.is_none() {
            t.tokens = load_tab_tokens(state_base, &t.name);
        }
    }
    Some(state)
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
    #[serde(
        default,
        deserialize_with = "deserialize_hotkeys",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hotkeys: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_editor: Option<String>,
    /// HTTP API listener port. Defaults to 7890 when absent; exposed as
    /// a preference so users who already have something on that port,
    /// or who want a different one across machines, can change it
    /// without recompiling. TLS listener uses `api_port + 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
}

pub const DEFAULT_API_PORT: u16 = 7890;

fn deserialize_hotkeys<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let raw: Vec<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
            serde_json::Value::String(s) => legacy_hotkey_id_to_keycode(&s),
            _ => None,
        })
        .collect())
}

fn legacy_hotkey_id_to_keycode(id: &str) -> Option<u8> {
    match id {
        "grave" => Some(49),
        "f1" => Some(67),
        "f11" => Some(95),
        "f12" => Some(96),
        "xf86calculator" => Some(148),
        _ => None,
    }
}

pub static DEFAULT_HOTKEYS: &[u8] = &[49, 148];

struct KeycodeInfo {
    keycode: u8,
    label: &'static str,
    gpui_key: &'static str,
}

static KEYCODE_TABLE: &[KeycodeInfo] = &[
    KeycodeInfo {
        keycode: 9,
        label: "Escape",
        gpui_key: "escape",
    },
    KeycodeInfo {
        keycode: 10,
        label: "1",
        gpui_key: "1",
    },
    KeycodeInfo {
        keycode: 11,
        label: "2",
        gpui_key: "2",
    },
    KeycodeInfo {
        keycode: 12,
        label: "3",
        gpui_key: "3",
    },
    KeycodeInfo {
        keycode: 13,
        label: "4",
        gpui_key: "4",
    },
    KeycodeInfo {
        keycode: 14,
        label: "5",
        gpui_key: "5",
    },
    KeycodeInfo {
        keycode: 15,
        label: "6",
        gpui_key: "6",
    },
    KeycodeInfo {
        keycode: 16,
        label: "7",
        gpui_key: "7",
    },
    KeycodeInfo {
        keycode: 17,
        label: "8",
        gpui_key: "8",
    },
    KeycodeInfo {
        keycode: 18,
        label: "9",
        gpui_key: "9",
    },
    KeycodeInfo {
        keycode: 19,
        label: "0",
        gpui_key: "0",
    },
    KeycodeInfo {
        keycode: 20,
        label: "-",
        gpui_key: "-",
    },
    KeycodeInfo {
        keycode: 21,
        label: "=",
        gpui_key: "=",
    },
    KeycodeInfo {
        keycode: 22,
        label: "Backspace",
        gpui_key: "backspace",
    },
    KeycodeInfo {
        keycode: 23,
        label: "Tab",
        gpui_key: "tab",
    },
    KeycodeInfo {
        keycode: 24,
        label: "Q",
        gpui_key: "q",
    },
    KeycodeInfo {
        keycode: 25,
        label: "W",
        gpui_key: "w",
    },
    KeycodeInfo {
        keycode: 26,
        label: "E",
        gpui_key: "e",
    },
    KeycodeInfo {
        keycode: 27,
        label: "R",
        gpui_key: "r",
    },
    KeycodeInfo {
        keycode: 28,
        label: "T",
        gpui_key: "t",
    },
    KeycodeInfo {
        keycode: 29,
        label: "Y",
        gpui_key: "y",
    },
    KeycodeInfo {
        keycode: 30,
        label: "U",
        gpui_key: "u",
    },
    KeycodeInfo {
        keycode: 31,
        label: "I",
        gpui_key: "i",
    },
    KeycodeInfo {
        keycode: 32,
        label: "O",
        gpui_key: "o",
    },
    KeycodeInfo {
        keycode: 33,
        label: "P",
        gpui_key: "p",
    },
    KeycodeInfo {
        keycode: 34,
        label: "[",
        gpui_key: "[",
    },
    KeycodeInfo {
        keycode: 35,
        label: "]",
        gpui_key: "]",
    },
    KeycodeInfo {
        keycode: 36,
        label: "Enter",
        gpui_key: "enter",
    },
    KeycodeInfo {
        keycode: 38,
        label: "A",
        gpui_key: "a",
    },
    KeycodeInfo {
        keycode: 39,
        label: "S",
        gpui_key: "s",
    },
    KeycodeInfo {
        keycode: 40,
        label: "D",
        gpui_key: "d",
    },
    KeycodeInfo {
        keycode: 41,
        label: "F",
        gpui_key: "f",
    },
    KeycodeInfo {
        keycode: 42,
        label: "G",
        gpui_key: "g",
    },
    KeycodeInfo {
        keycode: 43,
        label: "H",
        gpui_key: "h",
    },
    KeycodeInfo {
        keycode: 44,
        label: "J",
        gpui_key: "j",
    },
    KeycodeInfo {
        keycode: 45,
        label: "K",
        gpui_key: "k",
    },
    KeycodeInfo {
        keycode: 46,
        label: "L",
        gpui_key: "l",
    },
    KeycodeInfo {
        keycode: 47,
        label: ";",
        gpui_key: ";",
    },
    KeycodeInfo {
        keycode: 48,
        label: "'",
        gpui_key: "'",
    },
    KeycodeInfo {
        keycode: 49,
        label: "` (Grave)",
        gpui_key: "`",
    },
    KeycodeInfo {
        keycode: 51,
        label: "\\",
        gpui_key: "\\",
    },
    KeycodeInfo {
        keycode: 52,
        label: "Z",
        gpui_key: "z",
    },
    KeycodeInfo {
        keycode: 53,
        label: "X",
        gpui_key: "x",
    },
    KeycodeInfo {
        keycode: 54,
        label: "C",
        gpui_key: "c",
    },
    KeycodeInfo {
        keycode: 55,
        label: "V",
        gpui_key: "v",
    },
    KeycodeInfo {
        keycode: 56,
        label: "B",
        gpui_key: "b",
    },
    KeycodeInfo {
        keycode: 57,
        label: "N",
        gpui_key: "n",
    },
    KeycodeInfo {
        keycode: 58,
        label: "M",
        gpui_key: "m",
    },
    KeycodeInfo {
        keycode: 59,
        label: ",",
        gpui_key: ",",
    },
    KeycodeInfo {
        keycode: 60,
        label: ".",
        gpui_key: ".",
    },
    KeycodeInfo {
        keycode: 61,
        label: "/",
        gpui_key: "/",
    },
    KeycodeInfo {
        keycode: 65,
        label: "Space",
        gpui_key: "space",
    },
    KeycodeInfo {
        keycode: 67,
        label: "F1",
        gpui_key: "f1",
    },
    KeycodeInfo {
        keycode: 68,
        label: "F2",
        gpui_key: "f2",
    },
    KeycodeInfo {
        keycode: 69,
        label: "F3",
        gpui_key: "f3",
    },
    KeycodeInfo {
        keycode: 70,
        label: "F4",
        gpui_key: "f4",
    },
    KeycodeInfo {
        keycode: 71,
        label: "F5",
        gpui_key: "f5",
    },
    KeycodeInfo {
        keycode: 72,
        label: "F6",
        gpui_key: "f6",
    },
    KeycodeInfo {
        keycode: 73,
        label: "F7",
        gpui_key: "f7",
    },
    KeycodeInfo {
        keycode: 74,
        label: "F8",
        gpui_key: "f8",
    },
    KeycodeInfo {
        keycode: 75,
        label: "F9",
        gpui_key: "f9",
    },
    KeycodeInfo {
        keycode: 76,
        label: "F10",
        gpui_key: "f10",
    },
    KeycodeInfo {
        keycode: 95,
        label: "F11",
        gpui_key: "f11",
    },
    KeycodeInfo {
        keycode: 96,
        label: "F12",
        gpui_key: "f12",
    },
    KeycodeInfo {
        keycode: 107,
        label: "Print Screen",
        gpui_key: "print",
    },
    KeycodeInfo {
        keycode: 110,
        label: "Home",
        gpui_key: "home",
    },
    KeycodeInfo {
        keycode: 111,
        label: "Up",
        gpui_key: "up",
    },
    KeycodeInfo {
        keycode: 112,
        label: "Page Up",
        gpui_key: "pageup",
    },
    KeycodeInfo {
        keycode: 113,
        label: "Left",
        gpui_key: "left",
    },
    KeycodeInfo {
        keycode: 114,
        label: "Right",
        gpui_key: "right",
    },
    KeycodeInfo {
        keycode: 115,
        label: "End",
        gpui_key: "end",
    },
    KeycodeInfo {
        keycode: 116,
        label: "Down",
        gpui_key: "down",
    },
    KeycodeInfo {
        keycode: 117,
        label: "Page Down",
        gpui_key: "pagedown",
    },
    KeycodeInfo {
        keycode: 118,
        label: "Insert",
        gpui_key: "insert",
    },
    KeycodeInfo {
        keycode: 119,
        label: "Delete",
        gpui_key: "delete",
    },
    KeycodeInfo {
        keycode: 127,
        label: "Pause",
        gpui_key: "pause",
    },
    KeycodeInfo {
        keycode: 148,
        label: "XF86Calculator",
        gpui_key: "xf86calculator",
    },
];

#[must_use]
pub fn gpui_key_to_keycode(key: &str) -> Option<u8> {
    KEYCODE_TABLE.iter().find(|e| e.gpui_key == key).map(|e| e.keycode)
}

#[must_use]
pub fn keycode_label(keycode: u8) -> String {
    KEYCODE_TABLE
        .iter()
        .find(|e| e.keycode == keycode)
        .map_or_else(|| format!("Key {keycode}"), |e| e.label.to_string())
}

#[must_use]
pub fn load_preferences(config_base: &std::path::Path) -> Preferences {
    let path = config_dir(config_base).join("preferences.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

pub fn save_preferences(config_base: &std::path::Path, prefs: &Preferences) {
    let dir = config_dir(config_base);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("preferences.json");
    if let Ok(data) = serde_json::to_string_pretty(prefs) {
        let _ = std::fs::write(path, data);
    }
}

/// Atomically persist the tab list to `{config_base}/tab-atelier/tabs.json`.
///
/// Rotates `.bak`, `.bak.1`, `.bak.2`; staged via `.tmp` + fsync + rename.
/// Per-tab output should be saved separately with `save_tab_output()` so a
/// bad write to one tab's output cannot corrupt the global tab list.
pub fn save_state(config_base: &std::path::Path, state: &SavedState) {
    let dir = config_dir(config_base);
    let path = dir.join("tabs.json");
    write_atomic_with_rotation(&dir, &path, state);
}

pub fn save_preferences_at(path: &std::path::Path, prefs: &Preferences) {
    if let Some(parent) = path.parent() {
        write_atomic_with_rotation(parent, path, prefs);
    }
}

/// Persist a single tab's output buffer to its own file
/// (`{state_base}/tab-atelier/output_tab-<sanitized-name>.json`). Atomic,
/// with one rotated backup.
pub fn save_tab_output(state_base: &std::path::Path, tab_name: &str, output: &str) {
    let dir = state_dir(state_base);
    let path = tab_output_path(state_base, tab_name);
    write_atomic_with_rotation(&dir, &path, &output);
}

#[must_use]
pub fn load_tab_output(state_base: &std::path::Path, tab_name: &str) -> Option<String> {
    let path = tab_output_path(state_base, tab_name);
    if let Ok(data) = std::fs::read_to_string(&path)
        && let Ok(s) = serde_json::from_str::<String>(&data)
    {
        return Some(s);
    }
    let bak = path.with_extension("json.bak");
    if let Ok(data) = std::fs::read_to_string(&bak)
        && let Ok(s) = serde_json::from_str::<String>(&data)
    {
        return Some(s);
    }
    None
}

fn write_atomic_with_rotation<T: serde::Serialize>(dir: &std::path::Path, path: &std::path::Path, value: &T) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(dir);
    let Ok(data) = serde_json::to_string_pretty(value) else {
        return;
    };

    let tmp = path.with_extension("json.tmp");
    let Ok(mut f) = std::fs::File::create(&tmp) else { return };
    if f.write_all(data.as_bytes()).is_err() || f.sync_all().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(f);

    if path.exists() {
        let bak = path.with_extension("json.bak");
        let bak1 = path.with_extension("json.bak.1");
        let bak2 = path.with_extension("json.bak.2");
        let _ = std::fs::rename(&bak1, &bak2);
        let _ = std::fs::rename(&bak, &bak1);
        let _ = std::fs::rename(path, &bak);
    }
    let _ = std::fs::rename(&tmp, path);

    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
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
            // Pick up a leading `~` so home-relative paths like
            // `~/.local/state/tab-atelier/tabs.json` are detected as a whole.
            // Same for `$VAR/...` style env-var prefixes.
            if start > 0 && matches!(chars[start - 1], '~' | '$') {
                start -= 1;
            }
            let mut j = i;
            while j < len
                && !chars[j].is_whitespace()
                && !matches!(chars[j], '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '|' | '│')
            {
                j += 1;
            }
            while j > start + 1 && matches!(chars[j - 1], '.' | ',' | ';') {
                j -= 1;
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
            while j > start + 1 && matches!(chars[j - 1], '.' | ',' | ';') {
                j -= 1;
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

/// Strip ANSI CSI/SGR escapes (`ESC [ … final`) from `s`.
///
/// Used when copying scrollback to the system clipboard so the receiving
/// app doesn't see raw escape sequences. Persistence and the mobile API
/// endpoints keep colours intentionally and bypass this helper.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                // CSI parameters are `0x30..=0x3F`, intermediates `0x20..=0x2F`,
                // and the sequence ends at the first byte in `0x40..=0x7E`.
                if ('\x40'..='\x7e').contains(&nc) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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
    fn strip_ansi_removes_sgr_and_keeps_text() {
        let s = "\x1b[1;31mhello\x1b[0m, \x1b[32mworld\x1b[0m";
        assert_eq!(strip_ansi(s), "hello, world");
    }

    #[test]
    fn strip_ansi_handles_no_escapes_and_partial_sequence() {
        assert_eq!(strip_ansi("plain text"), "plain text");
        // Lone ESC without `[` is preserved verbatim.
        assert_eq!(strip_ansi("ab\x1bcd"), "ab\x1bcd");
    }

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
                    colors_enabled: true,
                    tokens: None,
                },
                TabState {
                    name: "Build".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                },
            ],
            active: 1,
            windowed: false,
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
    fn test_tab_state_colors_enabled_round_trip() {
        // false survives a round-trip; true is omitted from the JSON.
        let state = SavedState {
            tabs: vec![TabState {
                name: "dumb".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: false,
                tokens: None,
            }],
            active: 0,
            windowed: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains("\"colors_enabled\":false"),
            "expected colors_enabled=false in {json}",
        );
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert!(!restored.tabs[0].colors_enabled);

        // Missing field deserializes to the default (true).
        let restored: SavedState = serde_json::from_str(r#"{"tabs":[{"name":"x","cwd":null}],"active":0}"#).unwrap();
        assert!(restored.tabs[0].colors_enabled);
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
                colors_enabled: true,
                tokens: None,
            }],
            active: 0,
            windowed: false,
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
            windowed: false,
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
    fn test_crc32_matches_known_vector() {
        // "123456789" → 0xCBF43926 (standard CRC-32/ISO-HDLC test vector).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn test_sanitize_tab_filename_collision_resistant() {
        // "foo/bar" and "foo_bar" sanitize to the same prefix but the CRC
        // suffix keeps them distinct.
        let a = sanitize_tab_filename("foo/bar");
        let b = sanitize_tab_filename("foo_bar");
        assert!(a.starts_with("foo_bar-"));
        assert!(b.starts_with("foo_bar-"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_sanitize_tab_filename_handles_unusual_names() {
        assert!(sanitize_tab_filename("").starts_with("_-"));
        assert!(sanitize_tab_filename(".hidden").starts_with("_.hidden-"));
        let long = "a".repeat(200);
        let san = sanitize_tab_filename(&long);
        // Truncated to 100 + 1 ("-") + 8 (hex) = 109 chars max.
        assert!(san.len() <= 109);
    }

    #[test]
    fn test_save_tab_output_round_trip() {
        let base = std::env::temp_dir().join("ta-test-output-roundtrip");
        let _ = std::fs::remove_dir_all(&base);

        save_tab_output(&base, "build/run", "lots of output\nhere\n");
        let loaded = load_tab_output(&base, "build/run");
        assert_eq!(loaded.as_deref(), Some("lots of output\nhere\n"));

        // Same sanitized prefix, different CRC → independent file.
        save_tab_output(&base, "build_run", "different tab");
        assert_eq!(load_tab_output(&base, "build_run").as_deref(), Some("different tab"));
        // Original is untouched.
        assert_eq!(
            load_tab_output(&base, "build/run").as_deref(),
            Some("lots of output\nhere\n")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_save_rotates_backups() {
        let dir = std::env::temp_dir().join("ta-test-rotation");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let mk = |name: &str| SavedState {
            tabs: vec![TabState {
                name: name.into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
            }],
            active: 0,
            windowed: false,
        };

        save_state(&dir, &mk("v1"));
        save_state(&dir, &mk("v2"));
        save_state(&dir, &mk("v3"));
        save_state(&dir, &mk("v4"));

        let sd = state_dir(&dir);
        let read = |name: &str| {
            std::fs::read_to_string(sd.join(name))
                .ok()
                .and_then(|s| serde_json::from_str::<SavedState>(&s).ok())
                .and_then(|s| s.tabs.into_iter().next().map(|t| t.name))
        };

        assert_eq!(read("tabs.json").as_deref(), Some("v4"));
        assert_eq!(read("tabs.json.bak").as_deref(), Some("v3"));
        assert_eq!(read("tabs.json.bak.1").as_deref(), Some("v2"));
        assert_eq!(read("tabs.json.bak.2").as_deref(), Some("v1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_falls_back_to_bak_when_primary_corrupt() {
        let dir = std::env::temp_dir().join("ta-test-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        let sd = state_dir(&dir);
        let _ = std::fs::create_dir_all(&sd);

        let good = SavedState {
            tabs: vec![TabState {
                name: "rescued".into(),
                cwd: None,
                output: None,
                uptime_secs: None,
                energy_wh: None,
                colors_enabled: true,
                tokens: None,
            }],
            active: 0,
            windowed: false,
        };
        std::fs::write(sd.join("tabs.json"), "broken json").unwrap();
        std::fs::write(sd.join("tabs.json.bak"), serde_json::to_string(&good).unwrap()).unwrap();

        let loaded = load_state_from(&dir).expect("should fall back to .bak");
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].name, "rescued");

        let _ = std::fs::remove_dir_all(&dir);
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
                    colors_enabled: true,
                    tokens: None,
                },
                TabState {
                    name: "Two".into(),
                    cwd: None,
                    output: None,
                    uptime_secs: None,
                    energy_wh: None,
                    colors_enabled: true,
                    tokens: None,
                },
            ],
            active: 1,
            windowed: false,
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
                colors_enabled: true,
                tokens: None,
            }],
            active: 0,
            windowed: false,
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
    fn detect_file_path_trims_trailing_period() {
        let urls = detect_urls("deb at /tmp/pkg/app_0.1-1_amd64.deb.");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/tmp/pkg/app_0.1-1_amd64.deb");
    }

    #[test]
    fn detect_file_path_with_tilde() {
        let urls = detect_urls("see ~/.local/state/tab-atelier/tabs.json for state");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "~/.local/state/tab-atelier/tabs.json");
    }

    #[test]
    fn detect_file_path_with_tilde_at_start() {
        let urls = detect_urls("~/.config/foo/bar.txt");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "~/.config/foo/bar.txt");
    }

    #[test]
    fn detect_file_path_with_env_var_prefix() {
        let urls = detect_urls("see $XDG_STATE_HOME/tab-atelier/tabs.json after reboot");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "$XDG_STATE_HOME/tab-atelier/tabs.json");
    }

    #[test]
    fn detect_file_path_with_home_env_var() {
        let urls = detect_urls("$HOME/dev/foo/bar.rs");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "$HOME/dev/foo/bar.rs");
    }

    #[test]
    fn detect_file_path_does_not_eat_arbitrary_prefix() {
        // 'cat' before /tmp shouldn't be captured (only ~ is grafted on).
        let urls = detect_urls("cat /home/user/foo/bar");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].2, "/home/user/foo/bar");
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
                colors_enabled: true,
                tokens: None,
            }],
            active: 999,
            windowed: false,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(sd.join("tabs.json"), json).unwrap();

        let loaded = load_state_from(&dir).unwrap();
        assert_eq!(loaded.active, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpui_key_to_keycode_known_keys() {
        assert_eq!(gpui_key_to_keycode("`"), Some(49));
        assert_eq!(gpui_key_to_keycode("f12"), Some(96));
        assert_eq!(gpui_key_to_keycode("f1"), Some(67));
        assert_eq!(gpui_key_to_keycode("escape"), Some(9));
        assert_eq!(gpui_key_to_keycode("space"), Some(65));
        assert_eq!(gpui_key_to_keycode("a"), Some(38));
        assert_eq!(gpui_key_to_keycode("xf86calculator"), Some(148));
    }

    #[test]
    fn gpui_key_to_keycode_unknown() {
        assert_eq!(gpui_key_to_keycode("nonexistent"), None);
        assert_eq!(gpui_key_to_keycode(""), None);
        assert_eq!(gpui_key_to_keycode("F12"), None);
    }

    #[test]
    fn keycode_label_known() {
        assert_eq!(keycode_label(49), "` (Grave)");
        assert_eq!(keycode_label(96), "F12");
        assert_eq!(keycode_label(148), "XF86Calculator");
        assert_eq!(keycode_label(65), "Space");
    }

    #[test]
    fn keycode_label_unknown_fallback() {
        assert_eq!(keycode_label(200), "Key 200");
        assert_eq!(keycode_label(0), "Key 0");
        assert_eq!(keycode_label(255), "Key 255");
    }

    #[test]
    fn legacy_hotkey_ids() {
        assert_eq!(legacy_hotkey_id_to_keycode("grave"), Some(49));
        assert_eq!(legacy_hotkey_id_to_keycode("f1"), Some(67));
        assert_eq!(legacy_hotkey_id_to_keycode("f11"), Some(95));
        assert_eq!(legacy_hotkey_id_to_keycode("f12"), Some(96));
        assert_eq!(legacy_hotkey_id_to_keycode("xf86calculator"), Some(148));
        assert_eq!(legacy_hotkey_id_to_keycode("unknown"), None);
        assert_eq!(legacy_hotkey_id_to_keycode(""), None);
    }

    #[test]
    fn deserialize_hotkeys_numbers() {
        let json = r#"{"hotkeys": [49, 96, 148]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96, 148]);
    }

    #[test]
    fn deserialize_hotkeys_legacy_strings() {
        let json = r#"{"hotkeys": ["grave", "f12", "xf86calculator"]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96, 148]);
    }

    #[test]
    fn deserialize_hotkeys_mixed() {
        let json = r#"{"hotkeys": ["grave", 96]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 96]);
    }

    #[test]
    fn deserialize_hotkeys_empty() {
        let json = r#"{"hotkeys": []}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert!(prefs.hotkeys.is_empty());
    }

    #[test]
    fn deserialize_hotkeys_missing_field() {
        let json = r"{}";
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert!(prefs.hotkeys.is_empty());
    }

    #[test]
    fn deserialize_hotkeys_invalid_entries_skipped() {
        let json = r#"{"hotkeys": ["grave", "bogus", null, 300, 49]}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.hotkeys, vec![49, 49]);
    }
}
