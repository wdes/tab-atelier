// SPDX-License-Identifier: MPL-2.0

//! The agent card: the handful of meta labels that say what a tab is.
//!
//! There is no card *type* on disk — a card is a conventional set of keys in the
//! tab's `meta` map, which `set-meta` (and `POST /tabs`'s `meta`) already write.
//! This module is the one place that convention lives, so the dashboard and the
//! CLI read the same fields under the same names instead of each inventing them.
//!
//! That choice matters: a card as its own stored object would need its own
//! endpoint, its own validation, its own migration, and its own way of drifting
//! out of sync with `meta`. As keys, a card is set with the mechanism that
//! already exists and can be read off any tab the daemon already lists.

use std::collections::BTreeMap;

use serde::Serialize;

/// The meta keys that make up a card, in display order.
///
/// `current_task` not `currentTask`: meta keys are restricted to `[a-z0-9_-]`
/// by [`crate::sanitize_meta`]'s daemon-side twin, so a camel-cased key would
/// never survive the write.
pub const CARD_KEYS: [&str; 6] = [
    "assignment",
    "specialty",
    "orchestrator",
    "objective",
    "conventions",
    "current_task",
];

/// The card fields a tab carries, each absent when its key is unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentCard {
    /// What the agent is assigned to — the dashboard groups by this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment: Option<String>,
    /// The agent's role or trade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specialty: Option<String>,
    /// The agent that owns this one, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    /// What this agent is trying to achieve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// House rules the agent works under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conventions: Option<String>,
    /// What the agent is doing right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_task: Option<String>,
}

impl AgentCard {
    /// Whether the card carries anything at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.assignment.is_none()
            && self.specialty.is_none()
            && self.orchestrator.is_none()
            && self.objective.is_none()
            && self.conventions.is_none()
            && self.current_task.is_none()
    }
}

/// Read a card out of a tab's meta map, or `None` when the tab carries no card
/// keys at all (so a plain tab is not reported as an empty card).
#[must_use]
pub fn card_from_meta(meta: &BTreeMap<String, String>) -> Option<AgentCard> {
    let get = |key: &str| meta.get(key).cloned();
    let card = AgentCard {
        assignment: get("assignment"),
        specialty: get("specialty"),
        orchestrator: get("orchestrator"),
        objective: get("objective"),
        conventions: get("conventions"),
        current_task: get("current_task"),
    };
    if card.is_empty() { None } else { Some(card) }
}

#[cfg(test)]
mod tests {
    use super::{CARD_KEYS, card_from_meta};
    use std::collections::BTreeMap;

    /// A tab with no card keys is not a card — a plain tab must not be reported
    /// as an empty card.
    #[test]
    fn no_keys_is_no_card() {
        assert!(card_from_meta(&BTreeMap::new()).is_none());
        let mut unrelated = BTreeMap::new();
        unrelated.insert("project".to_string(), "app".to_string());
        assert!(card_from_meta(&unrelated).is_none(), "an unrelated label is not a card");
    }

    /// The card fields come from the conventional keys, and only the ones set.
    #[test]
    fn the_card_reads_its_keys() {
        let mut meta = BTreeMap::new();
        meta.insert("assignment".to_string(), "app-back:build".to_string());
        meta.insert("specialty".to_string(), "implementer".to_string());
        meta.insert("current_task".to_string(), "wire the kiosk".to_string());

        let card = card_from_meta(&meta).expect("a card");
        assert_eq!(card.assignment.as_deref(), Some("app-back:build"));
        assert_eq!(card.specialty.as_deref(), Some("implementer"));
        assert_eq!(card.current_task.as_deref(), Some("wire the kiosk"));
        assert!(
            card.orchestrator.is_none(),
            "unset fields stay absent, not empty strings"
        );
    }

    /// Every declared key is a legal meta key, so a card always survives the
    /// write that sets it.
    #[test]
    fn every_card_key_is_a_legal_meta_key() {
        for key in CARD_KEYS {
            assert!(key.len() <= 32, "{key} fits the key cap");
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
                "{key} is [a-z0-9_-], so set-meta accepts it"
            );
        }
    }
}
