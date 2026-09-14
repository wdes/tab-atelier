// SPDX-License-Identifier: MPL-2.0

//! Accounts: the people, their pins, and their keys.
//!
//! Every entry point here takes a request that has already been validated, so
//! the body of each is the decision itself — no parsing, and no path where a
//! malformed value reaches the store. The refusals live in
//! [`crate::http::requests`], next to the fields they are about.

use std::sync::Arc;

use crate::http::requests::compact::SetCompact;
use crate::http::requests::user::{AddUser, PinModel, PinProvider, SetDisabled, SetTools, SetWeight};
use crate::http::resources;
use crate::server::State;
use crate::transport::{Reply, json_of};
use crate::users::Store;

/// Every account, for the operator's table.
pub(crate) fn list(state: &Arc<State>) -> Reply {
    let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    json_of(200, &resources::AccountsResource::of(store.accounts()))
}

/// Creating an account.
pub(crate) fn add(state: &Arc<State>, req: &AddUser) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.add(&req.first_name, &req.last_name, &req.email) {
        Ok(a) => json_of(201, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(400, &e.to_string()),
    }
}

/// Deleting an account, and with it everything only that account could explain.
pub(crate) fn remove(state: &Arc<State>, who: &str) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.remove(who) {
        Ok(a) => {
            // Deleting someone forgets their history too, or "remove" would
            // leave their numbers on the dashboard forever with no name
            // attached to them.
            state
                .usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&a.id);
            json_of(200, &resources::RemovedAccountEnvelope::from(&a))
        }
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Pin an account to a provider, or clear the pin with an empty name.
pub(crate) fn set_provider(state: &Arc<State>, who: &str, req: &PinProvider) -> Reply {
    let wanted = req.provider.trim();
    // Validated HERE, where the registry is in hand, rather than in users.rs
    // which knows about people and not about destinations. A pin to a typo
    // would otherwise be a 503 nobody could explain.
    if !wanted.is_empty() {
        let why: Option<String> = {
            let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // A pin to a provider that can never be used is a 503 on every
            // request from that account, caused by an admin action somewhere
            // else. Refuse it here, where there is a message.
            reg.get(wanted).map_or_else(
                || Some(format!("no provider {wanted:?}")),
                |p| p.unusable_reason().as_deref().map(str::to_owned),
            )
        };
        if let Some(why) = why {
            return crate::http::problem(400, why);
        }
    }
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_provider(who, (!wanted.is_empty()).then_some(wanted)) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Pin an account to a single model, or clear the pin.
///
/// The model pin outranks the provider pin: choosing a model chooses the hop
/// that serves it.
pub(crate) fn set_model(state: &Arc<State>, who: &str, req: &PinModel) -> Reply {
    let wanted = req.model.trim();
    if !wanted.is_empty() {
        let known = {
            let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.providers
                .iter()
                .any(|p| p.models.iter().any(|m| m.id.eq_ignore_ascii_case(wanted)))
        };
        if !known {
            return failure(400, &format!("no model {wanted:?}"));
        }
    }
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_model(who, (!wanted.is_empty()).then_some(wanted)) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// The account's compaction level.
///
/// Per PERSON, not per provider: the operator reasoning about it is looking at
/// a person, and routing picks the hop per request — a level filed under a
/// provider silently means something else the moment that provider stops being
/// where the traffic goes. The hop still gets a say, because the harm is a
/// property of the hop; see [`compact_refusal_for`].
pub(crate) fn set_compact(state: &Arc<State>, who: &str, req: &SetCompact) -> Reply {
    let Some(level) = crate::compact::Compact::ALL
        .into_iter()
        .find(|c| c.as_str() == req.compact.trim())
    else {
        return failure(400, &format!("unknown compaction level {:?}", req.compact));
    };
    // Refused on the SERVER, not only by the option the UI disables. A setting
    // that cannot be correct should not be offerable, and the browser is not
    // the only way in — this API is authenticated, not private.
    if !level.is_none() {
        let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(why) = compact_refusal_for(state, &store, who) {
            return failure(400, &why);
        }
    }
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_compact(who, level) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Whether this account's level can be served by the hop that would take it.
///
/// A property of the destination, not of the request: the same level is fine
/// here and refused there, so the answer has to be asked of the registry.
pub(crate) fn compact_refusal_for(state: &Arc<State>, store: &Store, who: &str) -> Option<String> {
    let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // The hop's objection is level-independent — it is the breaker on
    // `compact != none` — so any non-`none` level stands in for the rest.
    let ask = |p: &crate::provider::Provider| p.compact_refusal(crate::compact::Compact::Tools);
    store.find(who).and_then(|a| a.provider.as_deref()).map_or_else(
        || reg.providers.iter().filter(|p| p.enabled).find_map(ask),
        |id| reg.get(id).and_then(ask),
    )
}

/// The account's tool policy, written whole.
///
/// Whole-object rather than merged field by field: the policy decides what an
/// agent may do, and a partial write would leave a shape that nobody chose and
/// that no version of the UI would have produced.
pub(crate) fn set_tools(state: &Arc<State>, who: &str, req: &SetTools) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_tools(who, req.policy.clone()) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Switching an account off, or back on.
pub(crate) fn set_disabled(state: &Arc<State>, who: &str, req: &SetDisabled) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_disabled(who, req.disabled()) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// How many times this account's key is drawn in the rotation.
pub(crate) fn set_weight(state: &Arc<State>, who: &str, req: &SetWeight) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_weight(who, req.weight()) {
        Ok(a) => json_of(200, &resources::AccountEnvelope::from(&a)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// An error as the API reports it, with the wording the store chose.
fn failure(status: u16, why: &str) -> Reply {
    crate::http::problem(status, why.to_owned())
}
