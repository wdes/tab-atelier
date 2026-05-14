// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub name: String,
    pub cwd: Option<String>,
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

    #[test]
    fn test_tab_state_serialization() {
        let state = SavedState {
            tabs: vec![
                TabState { name: "Terminal".into(), cwd: Some("/home/user".into()) },
                TabState { name: "Build".into(), cwd: None },
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
        let result = load_state();
        let _ = result;
    }

    #[test]
    fn test_dirs_or_default_has_swoop() {
        let dir = dirs_or_default();
        assert_eq!(dir.file_name().unwrap(), "swoop");
    }
}
