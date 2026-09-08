// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a request can be sent, and what lives there.
//!
//! The proxy chooses the model. A caller asks for one, but that name is a
//! statement about the KIND of work — something cheap and quick, something
//! that has to think — not an instruction about which endpoint to bill. The
//! caller cannot know which provider has capacity right now; the proxy can.
//!
//! So a provider is a place to send Anthropic-shaped traffic, and a model is
//! an entry in a [`Class`]. Routing picks a `(provider, model)` pair from the
//! class ([`crate::routing`]).
//!
//! # Why every provider here speaks the Anthropic wire format
//!
//! Claude Code speaks the Messages API, so that is the contract on the way in
//! and cannot change. Providers that speak the same shape — Anthropic itself,
//! the same models on Bedrock or Vertex, and the several third parties that
//! ship an Anthropic-compatible endpoint precisely to serve Claude Code — can
//! be swapped by changing a base URL, a credential and a model name. Nothing
//! is translated, so nothing is lost: tool use, prompt caching and extended
//! thinking all pass through untouched.
//!
//! That restriction is what makes rerouting *safe* rather than merely
//! possible. An OpenAI-shaped provider would need the request AND the
//! streamed response rewritten, and tool-call semantics do not survive that
//! trip intact — which for Claude Code, whose every turn is tool use, means a
//! reroute that silently breaks the client. When that adapter is written it
//! belongs behind its own [`Wire`] variant, and this comment is the reason it
//! is not simply "another base URL".
//!
//! # The capacity point
//!
//! Bedrock and Vertex serve the SAME models from DIFFERENT quota pools. That
//! is the whole reroute story: when the subscription's five-hour window is
//! spent, the work does not have to stop or get worse, it moves.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a model is for, rather than what it is called.
///
/// Routing happens within a class: a request that arrived asking for Opus is a
/// request for something in [`Class::Heavy`], and any heavy model with
/// capacity can serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Cheap and quick. Classification, formatting, short answers.
    Fast,
    /// The default working tier.
    Balanced,
    /// Slow, expensive, and better at hard problems.
    Heavy,
}

impl Class {
    /// Classes from cheapest up, so a search can walk them in order.
    pub const LADDER: [Self; 3] = [Self::Fast, Self::Balanced, Self::Heavy];

    /// The next class down, for when nothing in this one has capacity.
    #[must_use]
    pub const fn cheaper(self) -> Option<Self> {
        match self {
            Self::Heavy => Some(Self::Balanced),
            Self::Balanced => Some(Self::Fast),
            Self::Fast => None,
        }
    }
}

/// The request/response shape a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// The Anthropic Messages API — passed through untouched.
    #[default]
    Anthropic,
}

/// How to authenticate to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    /// The proxy host's own Claude login, refreshed as needed. No API key
    /// anywhere, which is why the subscription can be shared at all.
    ClaudeOauth,
    /// A key read from the environment at startup. Named rather than inlined:
    /// a credential in a config file is a credential in a backup.
    ApiKeyEnv { var: String },
}

/// One model a provider serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// The id to send upstream. Provider-specific — the same model is
    /// `claude-opus-5` at Anthropic and something longer on Bedrock.
    pub id: String,
    pub class: Class,
    /// Relative price, only ever compared with other entries. An absolute
    /// figure would be wrong within a month; the ORDER is what routing needs,
    /// and that is stable.
    #[serde(default = "one")]
    pub relative_cost: u32,
}

const fn one() -> u32 {
    1
}

/// Somewhere requests can go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub wire: Wire,
    /// No trailing slash. `/v1/messages` is appended.
    pub base_url: String,
    pub auth: Auth,
    pub models: Vec<Model>,
    /// Tried in ascending order when several can serve a class. The
    /// subscription is 0 because it is already paid for.
    #[serde(default)]
    pub preference: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn yes() -> bool {
    true
}

impl Provider {
    /// The cheapest model this provider has in a class, if any.
    #[must_use]
    pub fn model_for(&self, class: Class) -> Option<&Model> {
        self.models
            .iter()
            .filter(|m| m.class == class)
            .min_by_key(|m| m.relative_cost)
    }

    /// Whether its credential is actually present.
    ///
    /// A provider configured but unusable must be visibly unusable — routing
    /// to one whose key is missing produces a 401 from somewhere the operator
    /// was not looking.
    #[must_use]
    pub fn credential_ready(&self) -> bool {
        self.credential_ready_with(|v| std::env::var(v).ok())
    }

    /// The same question with the environment supplied.
    ///
    /// Tests use this rather than setting variables: `set_var` is `unsafe`
    /// (forbidden in this crate) and races every other test in the binary.
    #[must_use]
    pub fn credential_ready_with(&self, get: impl Fn(&str) -> Option<String>) -> bool {
        match &self.auth {
            Auth::ClaudeOauth => true,
            Auth::ApiKeyEnv { var } => get(var).is_some_and(|v| !v.trim().is_empty()),
        }
    }
}

/// Everywhere requests can go.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub providers: Vec<Provider>,
}

impl Default for Registry {
    /// Anthropic alone, on the host's own subscription — what the proxy did
    /// before any of this existed, expressed as configuration.
    fn default() -> Self {
        Self {
            providers: vec![Provider {
                id: "anthropic".to_owned(),
                wire: Wire::Anthropic,
                base_url: crate::egress::ANTHROPIC_BASE.to_owned(),
                auth: Auth::ClaudeOauth,
                preference: 0,
                enabled: true,
                models: vec![
                    Model {
                        id: "claude-haiku-4-5-20251001".to_owned(),
                        class: Class::Fast,
                        relative_cost: 1,
                    },
                    Model {
                        id: "claude-sonnet-5".to_owned(),
                        class: Class::Balanced,
                        relative_cost: 5,
                    },
                    Model {
                        id: "claude-opus-5".to_owned(),
                        class: Class::Heavy,
                        relative_cost: 25,
                    },
                ],
            }],
        }
    }
}

impl Registry {
    /// Load `providers.json`, or fall back to [`Registry::default`].
    ///
    /// A malformed file is NOT silently replaced with the default: that would
    /// quietly route everything back to the subscription an operator had
    /// deliberately configured away from. It is reported and the default is
    /// used only because refusing to start would be worse for a proxy whose
    /// job is to keep serving.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        // No file at all is the ordinary first run, not a problem.
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(r) if !r.providers.is_empty() => r,
            Ok(_) => {
                log::warn!("{}: no providers listed; using the built-in default", path.display());
                Self::default()
            }
            Err(e) => {
                log::error!(
                    "{}: {e} — USING THE BUILT-IN DEFAULT, which may not be what you configured",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write the current registry out, so a first run leaves an editable file
    /// rather than a mystery.
    ///
    /// # Errors
    /// The file could not be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Which class a requested model name belongs to.
    ///
    /// Looks through every provider's list first, so a name any of them serves
    /// is classified exactly. Falls back to matching the family in the name,
    /// because a client can ask for a dated or aliased id we have never seen
    /// and "unknown" would be a worse answer than "it says opus".
    #[must_use]
    pub fn class_of(&self, model: &str) -> Option<Class> {
        if let Some(m) = self.providers.iter().flat_map(|p| &p.models).find(|m| m.id == model) {
            return Some(m.class);
        }
        let lower = model.to_ascii_lowercase();
        for (needle, class) in [
            ("opus", Class::Heavy),
            ("sonnet", Class::Balanced),
            ("haiku", Class::Fast),
        ] {
            if lower.contains(needle) {
                return Some(class);
            }
        }
        None
    }

    /// Every usable `(provider, model)` for a class, best first.
    ///
    /// Ordered by the operator's preference then by cost, so the intended
    /// destination wins and price only breaks ties. Providers that are
    /// disabled or whose credential is missing are left out entirely rather
    /// than tried and failed.
    #[must_use]
    pub fn candidates(&self, class: Class) -> Vec<(&Provider, &Model)> {
        self.candidates_with(class, |v| std::env::var(v).ok())
    }

    /// [`Registry::candidates`] with the environment supplied, for tests.
    #[must_use]
    pub fn candidates_with(
        &self,
        class: Class,
        get: impl Fn(&str) -> Option<String> + Copy,
    ) -> Vec<(&Provider, &Model)> {
        let mut out: Vec<(&Provider, &Model)> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.credential_ready_with(get))
            .filter_map(|p| p.model_for(class).map(|m| (p, m)))
            .collect();
        out.sort_by_key(|(p, m)| (p.preference, m.relative_cost));
        out
    }

    /// A count per provider id, for diagnostics.
    #[must_use]
    pub fn summary(&self) -> BTreeMap<String, usize> {
        self.providers.iter().map(|p| (p.id.clone(), p.models.len())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_provider_registry() -> Registry {
        let mut r = Registry::default();
        // The same models from a different quota pool — the case the whole
        // reroute story exists for.
        r.providers.push(Provider {
            id: "bedrock".to_owned(),
            wire: Wire::Anthropic,
            base_url: "https://bedrock.example".to_owned(),
            auth: Auth::ApiKeyEnv {
                var: "TA_TEST_BEDROCK_KEY".to_owned(),
            },
            preference: 1,
            enabled: true,
            models: vec![
                Model {
                    id: "anthropic.claude-opus-5-v1:0".to_owned(),
                    class: Class::Heavy,
                    relative_cost: 30,
                },
                Model {
                    id: "anthropic.claude-haiku-4-5-v1:0".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
                },
            ],
        });
        r
    }

    #[test]
    fn a_requested_model_maps_to_the_kind_of_work_it_is() {
        let r = two_provider_registry();
        assert_eq!(r.class_of("claude-opus-5"), Some(Class::Heavy));
        assert_eq!(r.class_of("claude-sonnet-5"), Some(Class::Balanced));
        assert_eq!(r.class_of("claude-haiku-4-5-20251001"), Some(Class::Fast));
        // A provider-specific id is known exactly.
        assert_eq!(r.class_of("anthropic.claude-opus-5-v1:0"), Some(Class::Heavy));
        // A dated or aliased name we have never seen still classifies by
        // family — better than refusing to route it.
        assert_eq!(r.class_of("claude-opus-5-20260101"), Some(Class::Heavy));
        assert_eq!(r.class_of("some-other-vendors-model"), None);
    }

    /// A stand-in environment, so no test mutates the process's own.
    fn with_key(v: &str) -> Option<String> {
        (v == "TA_TEST_BEDROCK_KEY").then(|| "a-key".to_owned())
    }
    fn without_key(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn candidates_are_ordered_by_preference_then_cost() {
        let r = two_provider_registry();

        let heavy = r.candidates_with(Class::Heavy, with_key);
        assert_eq!(heavy.len(), 2, "both providers serve heavy work");
        assert_eq!(heavy[0].0.id, "anthropic", "the subscription is preference 0");
        assert_eq!(heavy[1].0.id, "bedrock");

        // Nobody serves balanced except anthropic.
        let balanced = r.candidates_with(Class::Balanced, with_key);
        assert_eq!(balanced.len(), 1);
        assert_eq!(balanced[0].1.id, "claude-sonnet-5");
    }

    /// A provider whose key is missing must not be offered. Routing to it
    /// produces a 401 from somewhere nobody was looking.
    #[test]
    fn a_provider_without_its_credential_is_not_a_candidate() {
        let r = two_provider_registry();
        let heavy = r.candidates_with(Class::Heavy, without_key);
        assert_eq!(heavy.len(), 1, "bedrock has no key, so it is not offered");
        assert_eq!(heavy[0].0.id, "anthropic");
    }

    #[test]
    fn a_disabled_provider_is_not_a_candidate() {
        let mut r = two_provider_registry();
        for p in &mut r.providers {
            if p.id == "bedrock" {
                p.enabled = false;
            }
        }
        assert_eq!(r.candidates_with(Class::Heavy, with_key).len(), 1);
    }

    #[test]
    fn the_default_registry_is_the_subscription_alone() {
        let r = Registry::default();
        assert_eq!(r.providers.len(), 1);
        assert_eq!(r.providers[0].auth, Auth::ClaudeOauth);
        // Every class is served, or a request could arrive with nowhere to go.
        for class in Class::LADDER {
            assert_eq!(
                r.candidates_with(class, without_key).len(),
                1,
                "no candidate for {class:?}"
            );
        }
    }

    #[test]
    fn the_class_ladder_only_goes_down() {
        assert_eq!(Class::Heavy.cheaper(), Some(Class::Balanced));
        assert_eq!(Class::Balanced.cheaper(), Some(Class::Fast));
        assert_eq!(Class::Fast.cheaper(), None);
    }

    #[test]
    fn a_registry_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!("ta-proxy-prov-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("providers.json");

        let r = two_provider_registry();
        r.save(&path).expect("save");
        let back = Registry::load(&path);
        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.summary()["bedrock"], 2);

        // A broken file falls back to the default rather than refusing to
        // start — but it is loud about it, because the default is not what the
        // operator configured.
        std::fs::write(&path, "{ not json").expect("write");
        assert_eq!(Registry::load(&path).providers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
