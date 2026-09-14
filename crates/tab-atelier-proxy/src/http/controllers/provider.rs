// SPDX-License-Identifier: MPL-2.0

//! Providers: the upstreams this proxy can talk to.
//!
//! Every entry point here takes a value that could only have been built from a
//! valid body — the checks are in [`crate::http::requests::provider`], and this
//! module is where they stop being checks and start being writes.

use std::sync::Arc;

use crate::http::registry_dir;
use crate::http::requests::provider::{RotateProviderKey, SaveProvider};
use crate::http::resources::status::{IdResource, OkResource};
use crate::provider;
use crate::server::State;
use crate::transport::{Reply, json_of};

/// Add a provider, edit one, or copy one beside itself.
///
/// The three intentions share a body because they share a form. Which one is
/// meant falls out of the values: a preset names a provider outright, `dup`
/// asks for a second row beside an existing one, and anything else is a save.
pub(crate) fn save(request: &SaveProvider, state: &Arc<State>) -> Reply {
    let dir = registry_dir(state);
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    // A second key for a provider that already has one. The preset names a
    // single provider, so a plain save would be an edit of the row already
    // there — the key pasted last would land on top of the one before it, and
    // the operator would be left holding one credential while believing they
    // had two. `dup` is them saying they mean a separate entry, which gets a
    // free name; that name is also what gives it its own key file.
    let mut new = if let Some(preset) = request.preset() {
        if request.duplicate() {
            preset.provider_for(&dir, reg.providers.iter().map(|x| x.id.as_str()))
        } else {
            preset.provider(&dir)
        }
    } else {
        let id = request.id.trim();
        let Ok(models) = provider::parse_models(&request.models) else {
            return crate::http::problem(400, "models: one name per line, no blanks");
        };
        provider::Provider {
            id: id.to_owned(),
            wire: provider::Wire::Anthropic,
            base_url: request.base_url.trim().trim_end_matches('/').to_owned(),
            auth: provider::Auth::ApiKeyFile {
                path: provider::provider_key_path(&dir, id).display().to_string(),
            },
            models,
            preference: request.preference(),
            enabled: true,
            peak: None,
        }
    };

    // On update, everything the form CANNOT express is preserved.
    //
    // The form shows a base URL, models and a key. It has no field for the
    // auth kind, the preference or the peak schedule — so a save used to
    // overwrite them with whatever the request implied. For `auth` that was
    // not cosmetic: saving the subscription's own row through the form rewrote
    // it from `claude_oauth` to `api_key_file`, which left it with no
    // credential, out of the candidate list, and the whole proxy falling back
    // to nothing. A partial view must save partially.
    //
    // `enabled` is the exception: the table DOES show it, so a request that
    // names it is taken at its word and one that does not is preserved. That
    // second half is what keeps the older form working.
    if let Some(old) = reg.get(&new.id) {
        new.preference = old.preference;
        new.enabled = request.enabled.unwrap_or(old.enabled);
        new.auth = old.auth.clone();
        new.peak.clone_from(&old.peak);
    } else if let Some(wanted) = request.enabled {
        new.enabled = wanted;
    }

    let id = new.id.clone();
    let key = request.key.trim();
    if !key.is_empty() {
        let path = provider::provider_key_path(&dir, &id);
        if let Err(e) = provider::write_provider_key(&path, key) {
            return crate::http::problem(500, format!("key file: {e}"));
        }
    }

    reg.upsert(new);
    let saved = reg.save(&state.registry_path);
    // Scoped: the lock must not be held across the log line below.
    drop(reg);
    if let Err(e) = saved {
        return crate::http::problem(500, format!("registry: {e}"));
    }
    log::info!("proxy: provider {id} saved");
    json_of(200, &IdResource::of(id))
}

/// Replace one provider's key, and nothing else.
pub(crate) fn rotate_key(id: &str, request: &RotateProviderKey, state: &Arc<State>) -> Reply {
    let auth = {
        let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.get(id).map(|p| p.auth.clone())
    };
    // WHICH kinds of provider can hold a key at all, checked before anything
    // is written.
    //
    // This used to accept a key for any provider and write it to a file. For a
    // `claude_oauth` provider that file is never read — the egress resolves
    // the host's own login instead — so the write silently did nothing and
    // left a live Anthropic credential on disk that no code path consults.
    // Worse than useless: an operator who later changed the auth kind would
    // find a key they had forgotten they pasted, suddenly in use.
    let Some(auth) = auth else {
        return crate::http::problem(404, "no such provider");
    };
    // Refused BEFORE anything is written, and refused on the server as well as
    // in the UI: a disabled button is a hint, and the API is reachable without
    // one.
    if let Some(reason) = auth.no_key_reason(id) {
        return crate::http::problem(400, reason);
    }
    let path = provider::provider_key_path(&registry_dir(state), id);
    if let Err(e) = provider::write_provider_key(&path, &request.key) {
        return crate::http::problem(500, format!("key file: {e}"));
    }
    // No restart: the credential is read per request, which is the whole
    // reason it lives in a file rather than in the service's environment.
    log::info!("proxy: provider {id} key rotated");
    json_of(200, &OkResource::yes())
}

/// Remove a provider, and unpin whoever had been routed to it.
///
/// The unpinning happens first and is not optional. A pin is a person saying
/// "send me there"; leaving one behind after the destination is gone is how a
/// working account becomes a 502 that no page explains.
pub(crate) fn remove(id: &str, state: &Arc<State>) -> Reply {
    let pinned_here: Vec<String> = {
        let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let names: Vec<String> = store
            .accounts()
            .iter()
            .filter(|a| a.provider.as_deref() == Some(id))
            .map(crate::users::Account::display_name)
            .collect();
        for who in &names {
            if let Err(e) = store.set_provider(who, None) {
                log::error!("proxy: could not unpin {who} from {id}: {e}");
            }
        }
        names
    };
    if !pinned_here.is_empty() {
        log::warn!(
            "proxy: provider {id} removed — unpinned {} (they had been routed there)",
            pinned_here.join(", ")
        );
    }

    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !reg.remove(id) {
        return crate::http::problem(404, "no such provider");
    }
    let saved = reg.save(&state.registry_path);
    drop(reg);
    if let Err(e) = saved {
        return crate::http::problem(500, format!("registry: {e}"));
    }
    json_of(200, &OkResource::yes())
}
