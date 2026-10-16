// SPDX-License-Identifier: MPL-2.0

//! Deciding what is an **agent** — the one question the fleet, the badge and the
//! broadcast all ask, answered in one place.
//!
//! A tab's `agent_kind` is its **session kind**, and that is `"bash"` for a plain
//! shell: the daemon's own `is_daemon_kind` (`src/lib.rs`) exists to tell *its*
//! kinds from an **external agent CLI**. So "has an `agent_kind`" is not "has an
//! agent" — a shell has one too. The distinction is which kinds are agents, and
//! it lives here so the fleet, the card badge and the broadcast cannot drift:
//! the broadcast once asked every tab with a kind to publish a card, and typed
//! the brief into bash sessions.

use serde_json::Value;

/// The session kinds that are an **external agent CLI** rather than a shell.
///
/// Mirrors the daemon's `catbus | claude | codex` set (`src/lib.rs`). `bash` and
/// the daemon's other own kinds are deliberately absent — they are terminals,
/// not agents.
pub const AGENT_KINDS: [&str; 3] = ["claude", "catbus", "codex"];

/// The agent kind of a tab, if it is one.
///
/// `None` for a shell, a daemon/brain tab, or a tab with no kind at all — so a
/// caller that wants "is an agent here" reads `agent_of(tab).is_some()`.
#[must_use]
pub fn agent_of(tab: &Value) -> Option<&str> {
    tab.get("agent_kind")
        .and_then(Value::as_str)
        .filter(|kind| AGENT_KINDS.contains(kind))
}

/// The tab is a **dead** agent: its session is anchored but the agent is gone
/// (the daemon says so through the `dead` led). It still carries a kind — it is
/// an "early try" whose agent exited — but there is no one to answer, so a
/// broadcast must skip it.
#[must_use]
pub fn is_dead(tab: &Value) -> bool {
    tab.get("led").and_then(Value::as_str) == Some("dead")
}

/// The ids of the tabs a broadcast should reach: a real agent, still alive.
///
/// Pure, so it is testable without a socket — and this is the function the
/// regression test pins, because the bug was exactly here: a bash tab has an
/// `agent_kind`, and it was being asked to publish a card.
#[must_use]
pub fn broadcast_targets(tabs: &[Value]) -> Vec<String> {
    tabs.iter()
        .filter(|tab| agent_of(tab).is_some() && !is_dead(tab))
        .filter_map(|tab| tab.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{agent_of, broadcast_targets};
    use serde_json::json;

    /// Only the external agent CLIs count; a shell's `agent_kind` does not.
    #[test]
    fn a_shell_is_not_an_agent() {
        assert_eq!(agent_of(&json!({ "agent_kind": "claude" })), Some("claude"));
        assert_eq!(agent_of(&json!({ "agent_kind": "catbus" })), Some("catbus"));
        assert_eq!(agent_of(&json!({ "agent_kind": "codex" })), Some("codex"));
        assert_eq!(
            agent_of(&json!({ "agent_kind": "bash" })),
            None,
            "a shell is not an agent"
        );
        assert_eq!(agent_of(&json!({ "agent_kind": "brain" })), None);
        assert_eq!(agent_of(&json!({})), None);
    }

    /// The regression: the broadcast once targeted every tab with an
    /// `agent_kind`, so bash sessions got the card-publishing brief. Now only a
    /// live external agent does — and a dead one ("early try", agent gone) is
    /// skipped too.
    #[test]
    fn the_broadcast_targets_only_live_agents() {
        let tabs = vec![
            json!({ "id": "bash-1", "agent_kind": "bash" }),
            json!({ "id": "claude-1", "agent_kind": "claude" }),
            json!({ "id": "catbus-1", "agent_kind": "catbus" }),
            json!({ "id": "brain-1", "agent_kind": "brain" }),
            json!({ "id": "dead-1", "agent_kind": "claude", "led": "dead" }),
            json!({ "id": "plain-1" }),
            // An agent kind with no id: nothing to address, so nothing to send to.
            json!({ "agent_kind": "claude" }),
        ];
        let targeted = broadcast_targets(&tabs);
        assert_eq!(targeted, vec!["claude-1".to_string(), "catbus-1".to_string()]);
        for excluded in ["bash-1", "brain-1", "dead-1", "plain-1"] {
            assert!(
                !targeted.iter().any(|id| id == excluded),
                "{excluded} must not be targeted"
            );
        }
    }
}
