// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Claude-to-Claude teamwork verbs over the local API.
//!
//! NOT the catbus agent framework (which coordinates its own agents) — these
//! are thin, session-safe wrappers so the plain `claude` tabs can see,
//! broadcast to, and hand files to each other:
//!
//!   - `peers`             — list sibling tabs (name / state / cwd / context)
//!   - `note` / `notes`    — an append-only shared blackboard all tabs can read
//!   - `handoff`           — drop a file into a peer tab's `inbox/`
//!
//! Sending a prompt to another agent and waiting for its answer already lives
//! in `tab-atelier dispatch` (see `cli::delegate`); this module is the rest.

use serde::Deserialize;

use crate::cli::share_link::{Endpoint, discover_endpoint, fetch_tabs};

/// The subset of a `/tabs` entry the team commands care about. Deserialised
/// from the JSON `fetch_tabs` returns.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TabView {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    /// "thinking" | "waiting" | "error" | absent. Absent ⇒ idle at a prompt.
    #[serde(default)]
    pub agent_state: Option<String>,
    /// "claude" | "catbus" | absent. Only Claude tabs are teammates.
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub locked: bool,
}

impl TabView {
    /// Human-facing status word for the LED state (`None` ⇒ "idle").
    #[must_use]
    pub fn state_word(&self) -> &str {
        match self.agent_state.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => "idle",
        }
    }
}

/// Fetch `/tabs` and deserialise into typed [`TabView`]s.
pub(crate) fn fetch_tab_views(ep: &Endpoint) -> Result<Vec<TabView>, String> {
    let raw = fetch_tabs(ep)?;
    raw.into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| format!("parse tab: {e}")))
        .collect()
}

/// Pick the tabs to show as teammates: Claude sessions only, unless `all`.
/// A Claude tab is one whose `agent_kind` is `"claude"`.
#[must_use]
pub fn select_peers(tabs: &[TabView], all: bool) -> Vec<&TabView> {
    tabs.iter()
        .filter(|t| all || t.agent_kind.as_deref() == Some("claude"))
        .collect()
}

/// One `peers` line: `[idx] name · state · cwd — context`.
#[must_use]
pub fn format_peer_line(t: &TabView) -> String {
    let lock = if t.locked { " 🔒" } else { "" };
    let ctx = match t.context.as_deref() {
        Some(c) if !c.is_empty() => format!(" — {c}"),
        _ => String::new(),
    };
    format!("[{}] {}{lock} · {} · {}{ctx}", t.index, t.name, t.state_word(), t.cwd)
}

/// `tab-atelier peers [--all]` — list sibling tabs so a Claude can pick a
/// collaborator (or wait on one) by name.
#[must_use]
pub fn peers(all: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("peers: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tab_views(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("peers: {e}");
            return 1;
        }
    };
    let sel = select_peers(&tabs, all);
    if sel.is_empty() {
        println!("(no {} tabs)", if all { "" } else { "Claude " });
        return 0;
    }
    for t in sel {
        println!("{}", format_peer_line(t));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(index: usize, name: &str, kind: Option<&str>, state: Option<&str>) -> TabView {
        TabView {
            index,
            id: format!("id-{index}"),
            name: name.into(),
            cwd: format!("/w/{name}"),
            agent_state: state.map(Into::into),
            agent_kind: kind.map(Into::into),
            agent_session_id: None,
            context: None,
            locked: false,
        }
    }

    #[test]
    fn select_peers_filters_to_claude_unless_all() {
        let tabs = vec![
            tab(0, "a", Some("claude"), None),
            tab(1, "sh", None, None),
            tab(2, "cb", Some("catbus"), None),
        ];
        let claude = select_peers(&tabs, false);
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].name, "a");
        // --all keeps everything, order preserved.
        let all = select_peers(&tabs, true);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn state_word_maps_none_to_idle() {
        assert_eq!(tab(0, "a", Some("claude"), None).state_word(), "idle");
        assert_eq!(tab(0, "a", Some("claude"), Some("")).state_word(), "idle");
        assert_eq!(tab(0, "a", Some("claude"), Some("thinking")).state_word(), "thinking");
    }

    #[test]
    fn format_peer_line_shows_index_name_state_cwd_and_context() {
        let mut t = tab(3, "db", Some("claude"), Some("thinking"));
        t.context = Some("migrations".into());
        assert_eq!(format_peer_line(&t), "[3] db · thinking · /w/db — migrations");
        // No context, locked → lock marker, no trailing dash.
        let mut l = tab(4, "ops", Some("claude"), None);
        l.locked = true;
        assert_eq!(format_peer_line(&l), "[4] ops 🔒 · idle · /w/ops");
    }

    #[test]
    fn tab_view_deserialises_partial_tabs_json() {
        // Only the fields we declared; extras ignored, missing default.
        let v: TabView =
            serde_json::from_str(r#"{"index":2,"id":"u","name":"n","agent_kind":"claude","extra":true}"#).unwrap();
        assert_eq!(v.index, 2);
        assert_eq!(v.name, "n");
        assert_eq!(v.agent_kind.as_deref(), Some("claude"));
        assert!(!v.locked);
        assert_eq!(v.agent_state, None);
    }
}
