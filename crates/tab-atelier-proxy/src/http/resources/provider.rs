// SPDX-License-Identifier: MPL-2.0

//! Providers as the API presents them.
//!
//! `Provider`, `Model`, `Peak` and `Mapping` already derive `Serialize`, and
//! the shapes below borrow from them rather than rebuilding them. That is the
//! point: the file on disk, the API response and — once okapi is wired — the
//! `OpenAPI` document are three views of one definition, so adding a field to a
//! provider cannot leave the UI or the spec behind.

use serde::Serialize;

use crate::compact::Compact;
use crate::provider::{self, Mapping, Peak, Provider};
use crate::server::State;
use crate::usage;

/// One model as offered through a provider.
///
/// `cost_now` is resolved rather than sent as `relative_cost` alone, because
/// the multiplier changes at the peak boundary and a client recomputing it
/// would need the same schedule arithmetic the server already has.
#[derive(Serialize)]
pub(crate) struct ModelResource {
    pub id: String,
    pub class: provider::Class,
    pub relative_cost: u32,
    pub cost_now: u32,
    /// What it costs, per 1M tokens, off peak.
    ///
    /// Sent beside `relative_cost` because the two answer different questions:
    /// `relative_cost` orders the hops for routing, this says what the bill
    /// will say. `None` where no figure is published — most hops — which the
    /// panel shows as unknown rather than as free.
    pub price: Option<provider::Price>,
    pub deprecated: bool,
    pub note: Option<String>,
}

/// One hop, as the panel sees it.
///
/// Never a credential — only whether one resolves.
#[derive(Serialize)]
pub(crate) struct ProviderResource {
    pub id: String,
    pub base_url: String,
    /// Which API this hop speaks. The UI reads it to offer the two flavours of
    /// a reasoning-capable model: on the `OpenAI` wire a request carrying tools
    /// must have reasoning forced off, so "tools" and "reasoning" are
    /// alternatives rather than both-at-once.
    pub wire: provider::Wire,
    pub preference: i32,
    pub enabled: bool,
    pub peak_now: bool,
    pub peak_until: Option<u64>,
    pub peak: Option<Peak>,
    /// Why the UI may not offer anything but `none` for traffic through here,
    /// if it may not. Level-independent: the harm is a property of the HOP, so
    /// any non-none level meets it equally, and the account's level decides
    /// whether the warning is shown. Sent so the reason is the server's, not a
    /// second copy of the rule in TypeScript.
    pub compact_refusal: Option<String>,
    /// Whether the credential resolves — never the credential.
    pub ready: bool,
    pub auth: &'static str,
    pub models: Vec<ModelResource>,
}

impl ProviderResource {
    /// `now` is passed in rather than read per provider, so every entry in a
    /// list agrees on the hour.
    fn of(p: &Provider, now: u64) -> Self {
        Self {
            id: p.id.clone(),
            base_url: p.base_url.clone(),
            wire: p.wire,
            preference: p.preference,
            enabled: p.enabled,
            peak_now: p.peak_now(now),
            peak_until: p.peak_until(now),
            peak: p.peak.clone(),
            compact_refusal: p.compact_refusal(Compact::Tools),
            ready: p.credential_ready(),
            auth: auth_name(&p.auth),
            models: p.models.iter().map(|m| model_resource(p, m, now)).collect(),
        }
    }
}

fn model_resource(p: &Provider, m: &provider::Model, now: u64) -> ModelResource {
    ModelResource {
        id: m.id.clone(),
        class: m.class,
        relative_cost: m.relative_cost,
        cost_now: p.cost_at(m, now),
        price: m.price,
        deprecated: m.deprecated,
        note: m.note.clone(),
    }
}

/// A preset, as offered before anyone commits to adding it.
#[derive(Serialize)]
pub(crate) struct PresetResource {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: String,
    pub configured: bool,
}

/// The compaction levels and their labels.
///
/// Sent so the wording lives in one place: the enum the routing reads is the
/// same one the UI renders.
#[derive(Serialize)]
pub(crate) struct CompactLevelResource {
    pub value: &'static str,
    pub label: &'static str,
}

/// Everything the providers panel needs, and no credential anywhere in it.
#[derive(Serialize)]
pub(crate) struct CatalogResource {
    pub providers: Vec<ProviderResource>,
    pub presets: Vec<PresetResource>,
    /// The model-name rewrites, as configured.
    pub mappings: Vec<Mapping>,
    pub compact_levels: Vec<CompactLevelResource>,
}

/// The whole providers page in one response.
///
/// The lock is held only for the copy and released before the value leaves:
/// nothing here borrows from the registry, so serialization happens outside it
/// and a slow write to the socket cannot stall a reader of the registry.
pub(crate) fn providers_json(state: &State) -> CatalogResource {
    let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = usage::now_secs();
    let catalog = CatalogResource {
        providers: reg.providers.iter().map(|p| ProviderResource::of(p, now)).collect(),
        presets: provider::Preset::ALL
            .into_iter()
            .map(|p| preset_resource(&reg, p))
            .collect(),
        mappings: reg.mappings.clone(),
        compact_levels: Compact::ALL
            .into_iter()
            .map(|c| CompactLevelResource {
                value: c.as_str(),
                label: c.label(),
            })
            .collect(),
    };
    drop(reg);
    catalog
}

/// A preset, and whether it has already been added.
///
/// `configured` is the only interesting part: it is what turns "add" into
/// "already here" in the panel, and it is answered by id so a preset that was
/// edited after being added still counts as present.
fn preset_resource(reg: &provider::Registry, p: provider::Preset) -> PresetResource {
    PresetResource {
        id: p.id(),
        label: p.label(),
        base_url: p.provider(std::path::Path::new("/")).base_url,
        configured: reg.get(p.id()).is_some(),
    }
}

/// How a provider authenticates, as the UI names it.
const fn auth_name(auth: &provider::Auth) -> &'static str {
    match auth {
        provider::Auth::ClaudeOauth => "claude_oauth",
        provider::Auth::ApiKeyEnv { .. } => "api_key_env",
        provider::Auth::ApiKeyFile { .. } => "api_key_file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Auth, Class, Registry, Wire};

    fn state_with(reg: Registry) -> State {
        let state = State::for_tests("t".to_owned());
        *state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = reg;
        state
    }

    #[test]
    fn every_way_of_authenticating_has_a_name_the_ui_can_read() {
        // The UI branches on these strings, so a variant falling through to a
        // default would silently present the wrong form.
        assert_eq!(auth_name(&Auth::ClaudeOauth), "claude_oauth");
        assert_eq!(auth_name(&Auth::ApiKeyEnv { var: "X".to_owned() }), "api_key_env");
        assert_eq!(auth_name(&Auth::ApiKeyFile { path: "/k".to_owned() }), "api_key_file");
    }

    #[test]
    fn a_preset_is_marked_configured_once_it_has_been_added() {
        // This is what turns "add" into "already here" in the panel, and it is
        // answered by id so a preset edited after being added still counts.
        let state = state_with(Registry::default());
        let before = providers_json(&state);
        let deepseek = before
            .presets
            .iter()
            .find(|p| p.id == "deepseek")
            .expect("the preset is offered");
        assert!(!deepseek.configured, "nothing has been added yet");

        {
            let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.providers
                .push(provider::Preset::Deepseek.provider(std::path::Path::new("/")));
        }
        let after = providers_json(&state);
        let deepseek = after
            .presets
            .iter()
            .find(|p| p.id == "deepseek")
            .expect("still offered");
        assert!(deepseek.configured, "it is configured now");
    }

    #[test]
    fn the_catalogue_carries_no_credential_only_whether_one_resolves() {
        // The whole point of this type: an operator looking at the panel must
        // not be able to read a key out of it, and a key that ends up here is
        // the kind of thing that gets pasted into a bug report.
        let mut reg = Registry::default();
        reg.providers[0] = Provider {
            auth: Auth::ApiKeyEnv {
                var: "MY_SECRET_VAR".to_owned(),
            },
            ..reg.providers[0].clone()
        };
        let json = serde_json::to_string(&providers_json(&state_with(reg))).expect("serialize");
        assert!(
            !json.contains("MY_SECRET_VAR"),
            "the env var name is a hint, not a secret, but it should not be needed"
        );
        assert!(json.contains("\"ready\""), "readiness is what is sent instead");
        assert!(json.contains("\"auth\""), "and how it authenticates");
    }

    #[test]
    fn every_compaction_level_is_offered_with_its_label() {
        // The wording lives here so the enum routing reads is the one the UI
        // renders; a missing entry is an empty dropdown.
        let catalog = providers_json(&state_with(Registry::default()));
        assert_eq!(catalog.compact_levels.len(), Compact::ALL.len());
        for level in Compact::ALL {
            assert!(
                catalog.compact_levels.iter().any(|c| c.value == level.as_str()),
                "{} is missing",
                level.as_str()
            );
        }
    }

    #[test]
    fn every_model_of_every_provider_is_listed() {
        // A model dropped in translation is one the UI cannot offer and the
        // operator cannot diagnose.
        let mut reg = Registry::default();
        let mut extra = reg.providers[0].clone();
        extra.id = "second".to_owned();
        extra.wire = Wire::Openai;
        reg.providers.push(extra);

        let catalog = providers_json(&state_with(reg));
        assert_eq!(catalog.providers.len(), 2);
        for p in &catalog.providers {
            assert!(!p.models.is_empty(), "{} lists no models", p.id);
        }
        assert_eq!(
            catalog.providers[1].wire,
            Wire::Openai,
            "which wire a hop speaks is what tells the UI how to treat its models"
        );
    }

    #[test]
    fn the_cost_shown_is_the_cost_at_the_moment_asked_about() {
        // `relative_cost` alone would be wrong during a peak window, and a
        // client recomputing it would need this same schedule arithmetic.
        let mut reg = Registry::default();
        reg.providers[0].peak = Some(Peak {
            multiplier_percent: 200,
            windows: Vec::new(),
            holidays: Vec::new(),
        });
        let catalog = providers_json(&state_with(reg));
        let m = catalog.providers[0]
            .models
            .iter()
            .find(|m| m.relative_cost > 0)
            .expect("a priced model");
        // No windows means peak never applies, so the two agree — which is the
        // half of the rule that does not depend on the clock.
        assert_eq!(m.cost_now, m.relative_cost);
    }

    #[test]
    fn the_class_of_a_model_survives_translation() {
        // Routing reads the class from the persisted provider; the UI reads it
        // from here. A mismatch would offer a heavy model for a fast job.
        let catalog = providers_json(&state_with(Registry::default()));
        let models = &catalog.providers[0].models;
        assert!(models.iter().any(|m| m.class == Class::Fast));
        assert!(models.iter().any(|m| m.class == Class::Balanced));
        assert!(models.iter().any(|m| m.class == Class::Heavy));
    }
}
