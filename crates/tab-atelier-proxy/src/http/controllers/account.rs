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
use crate::transport::{Doc, Reply};
use crate::users::Store;

/// Every account, for the operator's table.
pub(crate) fn list(state: &Arc<State>) -> Doc<resources::AccountsResource> {
    let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Doc::ok(resources::AccountsResource::of(store.accounts()))
}

/// Creating an account.
pub(crate) fn add(state: &Arc<State>, req: &AddUser) -> Result<Doc<resources::AccountEnvelope, 201>, Reply> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.add(&req.first_name, &req.last_name, &req.email) {
        Ok(a) => Ok(Doc::<resources::AccountEnvelope, 201>::ok(
            resources::AccountEnvelope::from(&a),
        )),
        Err(e) => Err(failure(400, &e.to_string())),
    }
}

/// Deleting an account, and with it everything only that account could explain.
pub(crate) fn remove(state: &Arc<State>, who: &str) -> Result<Doc<resources::RemovedAccountEnvelope>, Reply> {
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
            Ok(Doc::ok(resources::RemovedAccountEnvelope::from(&a)))
        }
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// Pin an account to a provider, or clear the pin with an empty name.
pub(crate) fn set_provider(
    state: &Arc<State>,
    who: &str,
    req: &PinProvider,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
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
            return Err(crate::http::problem(400, why));
        }
    }
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_provider(who, (!wanted.is_empty()).then_some(wanted)) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// Pin an account to a single model, or clear the pin.
///
/// The model pin outranks the provider pin: choosing a model chooses the hop
/// that serves it.
pub(crate) fn set_model(
    state: &Arc<State>,
    who: &str,
    req: &PinModel,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
    let wanted = req.model.trim();
    // Canonicalised to the catalogue's own spelling before it is stored, for the
    // reason the CLI does the same: routing resolves the pin by EXACT id, so a
    // value accepted in whatever case it arrived would be a 503 on every request
    // — the failure this check is here to prevent.
    let canonical = if wanted.is_empty() {
        None
    } else {
        let canonical = {
            let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.canonical_model(wanted)
                .map(str::to_owned)
                .ok_or_else(|| reg.refuses_model(wanted))
        };
        match canonical {
            Ok(id) => Some(id),
            Err(why) => return Err(failure(400, &why)),
        }
    };
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_model(who, canonical.as_deref()) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// The account's compaction level.
///
/// Per PERSON, not per provider: the operator reasoning about it is looking at
/// a person, and routing picks the hop per request — a level filed under a
/// provider silently means something else the moment that provider stops being
/// where the traffic goes. The hop still gets a say, because the harm is a
/// property of the hop; see [`compact_refusal_for`].
pub(crate) fn set_compact(
    state: &Arc<State>,
    who: &str,
    req: &SetCompact,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
    let Some(level) = crate::compact::Compact::ALL
        .into_iter()
        .find(|c| c.as_str() == req.compact.trim())
    else {
        return Err(failure(400, &format!("unknown compaction level {:?}", req.compact)));
    };
    // Refused on the SERVER, not only by the option the UI disables. A setting
    // that cannot be correct should not be offerable, and the browser is not
    // the only way in — this API is authenticated, not private.
    if !level.is_none() {
        let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(why) = compact_refusal_for(state, &store, who) {
            return Err(failure(400, &why));
        }
    }
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_compact(who, level) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
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
pub(crate) fn set_tools(
    state: &Arc<State>,
    who: &str,
    req: &SetTools,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_tools(who, req.policy.clone()) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// Switching an account off, or back on.
pub(crate) fn set_disabled(
    state: &Arc<State>,
    who: &str,
    req: &SetDisabled,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_disabled(who, req.disabled()) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// How many times this account's key is drawn in the rotation.
pub(crate) fn set_weight(
    state: &Arc<State>,
    who: &str,
    req: &SetWeight,
) -> Result<Doc<resources::AccountEnvelope>, Reply> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_weight(who, req.weight()) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// An error as the API reports it, with the wording the store chose.
fn failure(status: u16, why: &str) -> Reply {
    crate::http::problem(status, why.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::requests::user::SetDisabled;
    use crate::users::Store;

    /// A state of this test's own, with the seeded account in it.
    ///
    /// The store path is named after the test because it is written to disk: a
    /// shared one would have these tests inheriting each other's accounts, and
    /// "unknown account" would stop meaning what the test thinks it means.
    fn state_for(name: &str, email: &str) -> Arc<State> {
        // Per run as well as per test. Some of what is checked here cannot be
        // arranged from a store left over from a previous run — a key's
        // plaintext is only readable once — so every test starts from empty and
        // arranges exactly the state it needs.
        let dir = std::env::temp_dir()
            .join("tab-atelier-account-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut store = Store::load(dir.join("users.json")).expect("a fresh store");
        store.add("Ada", "Lovelace", email).expect("add");
        let state = Arc::new(State::for_tests("t".to_owned()));
        *state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = store;
        state
    }

    /// The status a handler answered, whichever side of the result it is.
    ///
    /// A creation cannot be a bare `Doc`: the same handler also answers 400, so
    /// it returns a `Result` and the test reads through it.
    fn answered<T, const CODE: u16>(reply: &Result<Doc<T, CODE>, Reply>) -> u16 {
        match reply {
            Ok(doc) => doc.status,
            Err(refusal) => refusal.status,
        }
    }

    fn add_user(first: &str, last: &str, email: &str) -> AddUser {
        AddUser {
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            email: email.to_owned(),
        }
    }

    // ── listing and creating ────────────────────────────────────────────────

    #[test]
    fn the_table_lists_every_account() {
        let state = state_for("list", "list@example.com");
        assert_eq!(list(&state).status, 200);
    }

    #[test]
    fn creating_an_account_answers_201() {
        // 201 and not 200: the client needs the id from the body to do anything
        // with the account it just made.
        let state = state_for("create", "create-existing@example.com");
        // The store is on disk and survives between runs, so the test arranges
        // its own precondition rather than assuming a clean one. Clearing first
        // is also the honest statement of what is being checked — creating an
        // account that is *not* there.
        let _ = remove(&state, "create@example.com");
        let reply = add(&state, &add_user("Grace", "Hopper", "create@example.com"));
        assert_eq!(answered(&reply), 201);
    }

    #[test]
    fn creating_an_account_that_already_exists_is_a_400() {
        // 400 rather than 409: the email is what is wrong with the request, and
        // the operator fixes it by sending a different one.
        let state = state_for("dup", "dup@example.com");
        let reply = add(&state, &add_user("Ada", "Lovelace", "dup@example.com"));
        assert_eq!(answered(&reply), 400);
    }

    // ── deleting ────────────────────────────────────────────────────────────

    #[test]
    fn deleting_an_account_answers_with_what_was_removed() {
        // The operator confirms it was the account they meant rather than one
        // whose email they mistyped.
        let state = state_for("delete", "delete@example.com");
        let reply = remove(&state, "delete@example.com");
        assert_eq!(answered(&reply), 200);
        assert!(
            state
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .find("delete@example.com")
                .is_none(),
            "and it is gone"
        );
    }

    #[test]
    fn deleting_an_account_that_is_not_there_is_a_404() {
        let state = state_for("delete-missing", "delete-missing@example.com");
        assert_eq!(answered(&remove(&state, "nobody@example.com")), 404);
    }

    // ── the provider pin ────────────────────────────────────────────────────

    #[test]
    fn pinning_to_a_provider_that_does_not_exist_is_refused_where_it_is_set() {
        // Validated on the way in rather than at relay time: a pin to a typo
        // would otherwise be a 503 on every request from that account, caused by
        // an admin action somewhere else and explainable by nobody.
        let state = state_for("pin-unknown", "pin-unknown@example.com");
        let reply = set_provider(
            &state,
            "pin-unknown@example.com",
            &PinProvider {
                provider: "not-a-provider".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 400);
    }

    #[test]
    fn an_empty_pin_clears_the_override() {
        // The escape hatch: an account that has been pinned has to be able to go
        // back to the installation default, and an empty name is how.
        let state = state_for("pin-clear", "pin-clear@example.com");
        let reply = set_provider(
            &state,
            "pin-clear@example.com",
            &PinProvider {
                provider: String::new(),
            },
        );
        assert_eq!(answered(&reply), 200);
    }

    #[test]
    fn pinning_an_account_that_does_not_exist_is_a_404() {
        let state = state_for("pin-missing", "pin-missing@example.com");
        let reply = set_provider(
            &state,
            "nobody@example.com",
            &PinProvider {
                provider: String::new(),
            },
        );
        assert_eq!(answered(&reply), 404);
    }

    // ── the model pin ───────────────────────────────────────────────────────

    #[test]
    fn pinning_to_a_model_no_provider_serves_is_refused() {
        // Same reasoning as the provider pin, and it is checked against every
        // provider rather than the account's own: which hop serves a model can
        // change, and the pin is a statement about the model.
        let state = state_for("model-unknown", "model-unknown@example.com");
        let reply = set_model(
            &state,
            "model-unknown@example.com",
            &PinModel {
                model: "not-a-model".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 400);
    }

    #[test]
    fn an_empty_model_pin_clears_the_override() {
        let state = state_for("model-clear", "model-clear@example.com");
        let reply = set_model(&state, "model-clear@example.com", &PinModel { model: String::new() });
        assert_eq!(answered(&reply), 200);
    }

    #[test]
    fn a_model_pin_is_stored_as_the_router_will_read_it() {
        // The route matched the id case-insensitively but routing reads it by
        // exact id, so accepting the caller's capitalisation stored a pin that
        // 503'd every request. Accepted — and written back as the catalogue
        // spells it, which is the value routing can resolve.
        let state = state_for("model-case", "model-case@example.com");
        let reply = set_model(
            &state,
            "model-case@example.com",
            &PinModel {
                model: "CLAUDE-SONNET-5".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 200);
        let stored = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("model-case@example.com")
            .and_then(|a| a.model.clone());
        assert_eq!(stored.as_deref(), Some("claude-sonnet-5"));
    }

    // ── compaction ──────────────────────────────────────────────────────────

    #[test]
    fn a_compaction_level_nobody_defines_is_refused() {
        // The UI offers only the known ones, but the API is authenticated rather
        // than private, so the server checks rather than trusting the browser.
        let state = state_for("compact-unknown", "compact-unknown@example.com");
        let reply = set_compact(
            &state,
            "compact-unknown@example.com",
            &SetCompact {
                compact: "maximum".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 400);
    }

    #[test]
    fn disabling_compaction_is_always_accepted() {
        // `none` is the escape hatch and skips the hop's objection entirely:
        // whatever a provider refuses, turning compaction off is never the thing
        // it is refusing.
        let state = state_for("compact-none", "compact-none@example.com");
        let reply = set_compact(
            &state,
            "compact-none@example.com",
            &SetCompact {
                compact: "none".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 200);
    }

    #[test]
    fn the_hop_is_asked_whether_it_can_serve_the_level() {
        // `compact_refusal_for` is the server-side half of the rule the UI also
        // enforces; the two disagreeing is how an account ends up with a level
        // its provider rejects on every request.
        let state = state_for("compact-hop", "compact-hop@example.com");
        let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Whichever way the default registry answers, the function has to answer
        // at all rather than panicking on a missing provider.
        let _ = compact_refusal_for(&state, &store, "compact-hop@example.com");
    }

    // ── the switches ────────────────────────────────────────────────────────

    #[test]
    fn switching_an_account_off_and_back_on_works() {
        let state = state_for("disable", "disable@example.com");
        for disabled in [true, false] {
            let reply = set_disabled(
                &state,
                "disable@example.com",
                &SetDisabled {
                    disabled: Some(disabled),
                },
            );
            assert_eq!(answered(&reply), 200, "disabled={disabled}");
        }
    }

    #[test]
    fn every_switch_refuses_an_account_that_is_not_there() {
        // One loop rather than four tests: the shared property is what matters,
        // and four copies would drift the moment a fifth switch is added.
        let state = state_for("switches", "switches@example.com");
        let missing = "nobody@example.com";

        assert_eq!(
            answered(&set_disabled(&state, missing, &SetDisabled { disabled: Some(true) })),
            404
        );
        assert_eq!(
            answered(&set_weight(&state, missing, &SetWeight { weight: Some(3) })),
            404
        );
        assert_eq!(
            answered(&set_tools(
                &state,
                missing,
                &SetTools {
                    policy: crate::tools::Policy::default()
                }
            )),
            404
        );
    }

    #[test]
    fn the_weight_that_was_set_is_the_weight_reported() {
        // The number is read by the scheduler; a response that echoed something
        // else would make the operator doubt which one is in force.
        let state = state_for("weight", "weight@example.com");
        let reply = set_weight(&state, "weight@example.com", &SetWeight { weight: Some(4) });
        assert_eq!(answered(&reply), 200);

        let weight = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("weight@example.com")
            .expect("the account")
            .weight;
        assert_eq!(weight, 4, "the number in force is the one that was set");
    }
}
