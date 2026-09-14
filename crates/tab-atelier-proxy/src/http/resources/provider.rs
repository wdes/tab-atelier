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
