// SPDX-License-Identifier: MPL-2.0

//! Accounts: the people, their pins, and their keys.
//!
//! Every entry point here takes a request that has already been validated, so
//! the body of each is the decision itself — no parsing, and no path where a
//! malformed value reaches the store. The refusals live in
//! [`crate::http::requests`], next to the fields they are about.

use std::sync::Arc;

use crate::http::requests::compact::SetCompact;
use crate::http::requests::user::SetKind;
use crate::http::requests::user::{AddUser, PinModel, PinProvider, SetDisabled, SetRoute, SetTools, SetWeight};
use crate::http::resources;
use crate::server::State;
use crate::transport::{Doc, Failure};
use crate::users::{self, Store};

/// Every account, for the operator's table.
pub(crate) fn list(state: &Arc<State>) -> Doc<resources::AccountsResource> {
    let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Doc::ok(resources::AccountsResource::of(store.accounts()))
}

/// Creating an account.
pub(crate) fn add(state: &Arc<State>, req: &AddUser) -> Result<Doc<resources::AccountEnvelope, 201>, Failure> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.add(&req.first_name, &req.last_name, &req.email) {
        Ok(a) => Ok(Doc::<resources::AccountEnvelope, 201>::ok(
            resources::AccountEnvelope::from(&a),
        )),
        Err(e) => Err(failure(400, &e.to_string())),
    }
}

/// Deleting an account, and with it everything only that account could explain.
pub(crate) fn remove(state: &Arc<State>, who: &str) -> Result<Doc<resources::RemovedAccountEnvelope>, Failure> {
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
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
            return Err(Failure::problem(400, why));
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
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

/// Declare what kind of traffic this account produces, or clear it.
///
/// The only way to reach `plan`. No field in a request body identifies plan mode:
/// the plan tools are declared in every Claude Code request regardless of
/// permission mode, so a detector keyed on them classified all traffic as plan and
/// would have routed real, writing work to whatever cheap model it pointed at.
/// An operator saying which account does their planning is honest about where the
/// decision came from.
pub(crate) fn set_kind(
    state: &Arc<State>,
    who: &str,
    req: &SetKind,
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
    let kind = match req.kind.trim().to_ascii_lowercase().as_str() {
        // Empty clears: the account goes back to being classified from its body.
        "" => None,
        "work" => Some(crate::classifier::Kind::Work),
        "classifier" | "auto" | "auto_mode" => Some(crate::classifier::Kind::Classifier),
        "plan" | "plan_mode" => Some(crate::classifier::Kind::Plan),
        other => {
            return Err(failure(
                400,
                &format!("kind: {other} is not work, classifier (alias: auto) or plan"),
            ));
        }
    };
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_kind_override(who, kind) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// Add, replace, or remove one of an account's per-person model routes.
///
/// A route says "when THIS account asks for X, serve Y". It is the one thing
/// that overrules the account's provider pin, which is otherwise absolute:
/// without that exception, "force this person onto one provider, except send
/// their Opus traffic elsewhere" cannot be expressed at all. See
/// `users::Account::routes`.
///
/// Both ends are canonicalised against the catalogue before storage, for the
/// same reason the model pin is: routing resolves them by EXACT id, so a value
/// stored in whatever case it arrived would 503 every request it matched. `from`
/// is canonicalised only when the catalogue knows it — a route may legitimately
/// rename a model this proxy does not serve, which is how an operator catches
/// traffic for a retired id.
pub(crate) fn set_route(
    state: &Arc<State>,
    who: &str,
    req: &SetRoute,
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
    let from = req.from.trim();
    let to_model = req.to_model.trim();

    // The kind decides which traffic this rule is about. `auto` is accepted
    // because that is what the feature is called where the operator met it.
    let kind = match req.kind.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "work" => Some(crate::classifier::Kind::Work),
        "classifier" | "auto" | "auto_mode" => Some(crate::classifier::Kind::Classifier),
        "plan" | "plan_mode" => Some(crate::classifier::Kind::Plan),
        other => {
            return Err(failure(
                400,
                &format!("kind: {other} is not work, classifier (alias: auto) or plan"),
            ));
        }
    };

    // The tool the rule selects on, if any. Normalised to the family for the
    // same reason `inspect::server_tool` is: a rule pinned to
    // `web_search_20250305` would stop applying the day the version moves.
    let tool = match req.tool.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "web_search" | "web_search_20250305" | "web_search_20260209" => Some("web_search".to_owned()),
        "web_fetch" => Some("web_fetch".to_owned()),
        other => {
            return Err(failure(400, &format!("tool: {other} is not web_search or web_fetch")));
        }
    };

    // Empty destination removes the route that these conditions identify.
    if to_model.is_empty() {
        let removal = users::ModelRoute {
            from: (!from.is_empty()).then(|| from.to_owned()),
            kind,
            from_provider: Some(req.from_provider.trim().to_owned()).filter(|p| !p.is_empty()),
            tool,
            to_model: String::new(),
            to_provider: None,
            note: None,
        };
        let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        return match store.remove_route(who, &removal) {
            Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
            Err(e) => Err(failure(404, &e.to_string())),
        };
    }

    let resolved = {
        let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(to) = reg.canonical_model(to_model).map(str::to_owned) else {
            return Err(failure(400, &reg.refuses_model(to_model)));
        };
        // A source provider has to exist: a rule restricted to a provider that is
        // not configured matches nothing, which is the same trap as a route that
        // never fires.
        let from_provider = Some(req.from_provider.trim().to_owned()).filter(|p| !p.is_empty());
        if let Some(id) = from_provider.as_deref().filter(|id| reg.get(id).is_none()) {
            return Err(failure(400, &format!("no provider {id}")));
        }
        // A named provider has to exist AND serve the destination, or the route
        // is a 503 waiting to happen the next time this person sends anything.
        let to_provider = match req.to_provider.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            None => None,
            Some(id) => match reg.get(id) {
                None => return Err(failure(400, &format!("no provider {id}"))),
                Some(p) if p.serves(&to).is_none() => {
                    return Err(failure(400, &format!("{id} does not serve {to}")));
                }
                Some(_) => Some(id.to_owned()),
            },
        };
        // Resolved while the guard is still held, then released. Both ends use the
        // registry, so this has to be before the drop rather than inline in the
        // struct below.
        let from_model =
            (!from.is_empty()).then(|| reg.canonical_model(from).map_or_else(|| from.to_owned(), str::to_owned));
        // The guard has done its work. Released here rather than at the end of the
        // block so the registry lock is not held across the store write below —
        // two locks in one function, and taking the second while holding the first
        // is how a lock-order bug gets written.
        drop(reg);
        users::ModelRoute {
            from: from_model,
            kind,
            from_provider,
            tool,
            to_model: to,
            to_provider,
            note: req
                .note
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
        }
    };

    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_route(who, resolved) {
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
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
) -> Result<Doc<resources::AccountEnvelope>, Failure> {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_weight(who, req.weight()) {
        Ok(a) => Ok(Doc::ok(resources::AccountEnvelope::from(&a))),
        Err(e) => Err(failure(404, &e.to_string())),
    }
}

/// An error as the API reports it, with the wording the store chose.
fn failure(status: u16, why: &str) -> Failure {
    Failure::problem(status, why)
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
    fn answered<T, const CODE: u16>(reply: &Result<Doc<T, CODE>, Failure>) -> u16 {
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
                model: "CLAUDE-SONNET-5-5".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 200);
        let stored = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("model-case@example.com")
            .and_then(|a| a.model.clone());
        assert_eq!(stored.as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn pinning_to_a_retired_model_is_refused_rather_than_quietly_redirected() {
        // A pin is exact by contract — "a pin nothing serves returns None and the
        // caller reports 503" — so a name the catalogue has moved past must be
        // refused at write time, not resolved to its successor. Silently pinning
        // to a different model than the operator typed is the failure this
        // guards: `claude-sonnet-5` became `claude-sonnet-5-5`, and a pin that
        // looked accepted but named the old id would 503 on every request.
        //
        // Note the asymmetry with *routing*: a client asking for the retired name
        // is still served (routing reads a family substring), because a client is
        // expressing a preference. A pin is an instruction, and an unsatisfiable
        // instruction should fail where it can still be read by a human.
        let state = state_for("model-retired", "model-retired@example.com");
        let reply = set_model(
            &state,
            "model-retired@example.com",
            &PinModel {
                model: "claude-sonnet-5".to_owned(),
            },
        );
        assert_eq!(answered(&reply), 400);
    }

    // ── per-person model routes ─────────────────────────────────────────────

    /// A state whose registry has a second provider, so routes have somewhere
    /// to point.
    ///
    /// `state_for` starts from `Registry::default()`, which is Anthropic alone
    /// — deliberately, it is what the proxy was before configuration existed.
    /// Every route test below is about pointing traffic at a *different*
    /// provider, so they need one that exists.
    fn state_with_deepseek(name: &str, email: &str) -> Arc<State> {
        let state = state_for(name, email);
        {
            let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut deepseek = crate::provider::Preset::Deepseek.provider(std::path::Path::new("/tmp/providers.json"));
            deepseek.auth = crate::provider::Auth::ApiKeyEnv { var: "K".to_owned() };
            reg.providers.push(deepseek);
        }
        state
    }

    fn route(from: &str, to: &str, provider: Option<&str>) -> SetRoute {
        SetRoute {
            from: from.to_owned(),
            kind: String::new(),
            from_provider: String::new(),
            tool: String::new(),
            to_model: to.to_owned(),
            to_provider: provider.map(str::to_owned),
            note: None,
        }
    }

    /// A route with conditions of the caller's choosing, for the finer-grained
    /// cases — `kind` and `from_provider` are the whole point of them.
    fn cond_route(from: &str, kind: &str, from_provider: &str, to: &str, provider: Option<&str>) -> SetRoute {
        SetRoute {
            from: from.to_owned(),
            kind: kind.to_owned(),
            from_provider: from_provider.to_owned(),
            tool: String::new(),
            to_model: to.to_owned(),
            to_provider: provider.map(str::to_owned),
            note: None,
        }
    }

    /// A route selecting on a server-side tool, which is how search traffic is
    /// sent somewhere other than the conversation that asked for it.
    fn tool_route(from: &str, tool: &str, to: &str) -> SetRoute {
        SetRoute {
            from: from.to_owned(),
            tool: tool.to_owned(),
            to_model: to.to_owned(),
            ..cond_route("", "", "", to, None)
        }
    }

    /// The routes an account ends up carrying, read back from the store.
    fn routes_of(state: &Arc<State>, email: &str) -> Vec<users::ModelRoute> {
        let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        store.find(email).expect("the account").routes.clone()
    }

    /// A route is stored against the account, resolved to catalogue ids.
    ///
    /// Resolved on the way in for the same reason the model pin is: routing
    /// matches a route's `from` exactly, so a value stored in whatever case it
    /// arrived would be a rule that never fires — the worst kind, because it
    /// looks configured.
    #[test]
    fn a_route_is_stored_resolved() {
        let state = state_with_deepseek("route-store", "route-store@example.com");

        let reply = set_route(
            &state,
            "route-store@example.com",
            &route("CLAUDE-OPUS-5-5", "DEEPSEEK-FLASH", Some("deepseek")),
        );
        assert_eq!(answered(&reply), 200);

        let routes = routes_of(&state, "route-store@example.com");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].from.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(routes[0].to_model, "deepseek-flash");
        assert_eq!(routes[0].to_provider.as_deref(), Some("deepseek"));
    }

    /// Setting the same `from` twice edits the route instead of stacking one.
    ///
    /// the conditions are the identity, so a second route for the same source
    /// would be dead weight that no one could see the effect of — and the
    /// operator, seeing two entries, would reasonably conclude the second wins.
    #[test]
    fn setting_the_same_source_twice_replaces_it() {
        let state = state_with_deepseek("route-replace", "route-replace@example.com");
        let who = "route-replace@example.com";

        set_route(&state, who, &route("claude-opus-5-5", "deepseek-flash", None)).expect("the first route is valid");
        // A destination this registry actually serves — `gpt-5.6-luna` is not in
        // it, and would be refused, leaving the first route in place and making
        // this test pass for the wrong reason.
        set_route(&state, who, &route("claude-opus-5-5", "claude-sonnet-5-5", None))
            .expect("the second route is valid");

        let routes = routes_of(&state, who);
        assert_eq!(routes.len(), 1, "one route per source");
        assert_eq!(routes[0].to_model, "claude-sonnet-5-5");
    }

    /// An empty destination removes the route, so one verb covers undo.
    #[test]
    fn an_empty_destination_removes_the_route() {
        let state = state_with_deepseek("route-remove", "route-remove@example.com");
        let who = "route-remove@example.com";

        set_route(&state, who, &route("claude-opus-5-5", "deepseek-flash", None)).expect("the route is valid");
        let reply = set_route(&state, who, &route("claude-opus-5-5", "", None));

        assert_eq!(answered(&reply), 200);
        assert!(routes_of(&state, who).is_empty());
    }

    /// A destination no provider serves is refused where a human can read it.
    ///
    /// This is the difference between a 400 now and a 503 on every request the
    /// moment that person sends traffic — and per-person routes do NOT fall
    /// back, so an unchecked destination is an outage waiting for its first
    /// request.
    #[test]
    fn a_destination_nothing_serves_is_refused() {
        let state = state_with_deepseek("route-bad-model", "route-bad-model@example.com");

        let reply = set_route(
            &state,
            "route-bad-model@example.com",
            &route("claude-opus-5-5", "no-such-model", None),
        );

        assert_eq!(answered(&reply), 400);
        assert!(routes_of(&state, "route-bad-model@example.com").is_empty());
    }

    /// A named provider that does not serve the destination is refused.
    ///
    /// The pair is checked as a pair: either half alone is servable, so a route
    /// naming both would pass a per-field check and fail on every request.
    #[test]
    fn a_provider_that_does_not_serve_the_destination_is_refused() {
        let state = state_with_deepseek("route-bad-pair", "route-bad-pair@example.com");

        let reply = set_route(
            &state,
            "route-bad-pair@example.com",
            &route("claude-opus-5-5", "deepseek-flash", Some("anthropic")),
        );

        assert_eq!(answered(&reply), 400);
        assert!(routes_of(&state, "route-bad-pair@example.com").is_empty());
    }

    /// A route to the same model is refused as the no-op it is.
    ///
    /// Accepting it would fill the file with rules that change nothing, and an
    /// operator reading them later would believe something was in effect.
    /// A route to the same model is refused as the no-op it is.
    ///
    /// Asserted against `validate`, which is where the rule lives: `accept` is the
    /// transport boundary and a direct controller call does not go through it, so
    /// a test of the controller would exercise the storage path and pass while the
    /// rule sat unused. Test each rule where it is enforced — this one at the
    /// boundary, the resolution rules in the controller below.
    #[test]
    fn a_route_to_the_same_model_is_refused() {
        use crate::http::requests::Validated as _;

        let same = route("claude-opus-5-5", "claude-opus-5-5", None);
        assert!(same.validate().is_err());

        // Case-insensitively too: the ids are canonicalised, so a route that
        // differs only in case is still a no-op.
        let cased = route("CLAUDE-OPUS-5-5", "claude-opus-5-5", None);
        assert!(cased.validate().is_err());

        // And a route that names nothing is refused, because `from` is its key.
        assert!(route("", "deepseek-flash", None).validate().is_err());
        // But a `from` is not the only way to satisfy that rule.
        assert!(cond_route("", "auto", "", "deepseek-flash", None).validate().is_ok());

        // A real route passes.
        assert!(route("claude-opus-5-5", "deepseek-flash", None).validate().is_ok());
    }

    /// A route survives being written and read back, which is what makes it
    /// outlive the process.
    #[test]
    fn routes_round_trip_through_the_store() {
        let state = state_with_deepseek("route-reload", "route-reload@example.com");
        let who = "route-reload@example.com";
        set_route(
            &state,
            who,
            &route("claude-opus-5-5", "deepseek-flash", Some("deepseek")),
        )
        .expect("the route is valid");

        let path = {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.path().to_path_buf()
        };
        let reloaded = Store::load(path).expect("the store reloads");

        let routes = &reloaded.find(who).expect("the account").routes;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].from.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(routes[0].to_provider.as_deref(), Some("deepseek"));
    }

    // ── route conditions through the endpoint ───────────────────────────────

    /// `auto` is accepted where the operator meets the feature by that name.
    ///
    /// Claude Code's UI calls it auto mode; the internal kind is `classifier`.
    /// Accepting both spellings is not politeness — a rule written with a name
    /// the tool rejects is a rule that does not get written.
    #[test]
    fn the_kind_accepts_auto_as_a_spelling_of_classifier() {
        let state = state_with_deepseek("route-alias", "route-alias@example.com");
        let who = "route-alias@example.com";

        for spelling in ["auto", "classifier", "auto_mode", "AUTO"] {
            let reply = set_route(
                &state,
                who,
                &cond_route("", spelling, "", "deepseek-flash", Some("deepseek")),
            );
            assert_eq!(answered(&reply), 200, "{spelling} should be accepted");
        }

        // All four spellings are the same rule, so they replace one another
        // rather than stacking four identical entries.
        let routes = routes_of(&state, who);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].kind, Some(crate::classifier::Kind::Classifier));
    }

    /// An unknown kind is refused rather than silently treated as work.
    ///
    /// Guessing here would be the worst outcome: a typo that quietly produced a
    /// work-only rule would leave the auto-mode judge moving when the operator
    /// meant to contain it, or the reverse.
    #[test]
    fn an_unknown_kind_is_refused() {
        let state = state_with_deepseek("route-kind-bad", "route-kind-bad@example.com");

        let reply = set_route(
            &state,
            "route-kind-bad@example.com",
            &cond_route("", "classifer", "", "deepseek-flash", None),
        );

        assert_eq!(answered(&reply), 400);
    }

    /// A rule with no conditions at all is refused.
    ///
    /// It would match every request in the system — including the auto-mode
    /// judge — which is a provider pin plus a model pin wearing a rule's clothes,
    /// and a typo away from the exact leak the kind rule prevents.
    #[test]
    fn a_rule_with_no_conditions_is_refused() {
        // At the boundary, where the rule is enforced. `accept` is what a real
        // request goes through; a controller call does not, so asserting here
        // would pass while the check sat unused — the mistake this file has made
        // once already.
        use crate::http::requests::Validated as _;

        assert!(cond_route("", "", "", "deepseek-flash", None).validate().is_err());

        // Any one condition is enough to make it a rule.
        assert!(cond_route("", "auto", "", "deepseek-flash", None).validate().is_ok());
        assert!(
            cond_route("claude-opus-5-5", "", "", "deepseek-flash", None)
                .validate()
                .is_ok()
        );
        assert!(
            cond_route("", "", "anthropic", "deepseek-flash", None)
                .validate()
                .is_ok()
        );

        // Removing a route has no conditions to require: the empty destination
        // plus whatever identifies it is the whole instruction.
        assert!(cond_route("", "", "", "", None).validate().is_ok());
    }

    /// A `from_provider` that is not configured is refused.
    ///
    /// A rule restricted to a provider that does not exist matches nothing —
    /// configured, visible, and dead. That is the same trap as a route that
    /// never fires, and it is checkable here.
    #[test]
    fn an_unknown_source_provider_is_refused() {
        let state = state_with_deepseek("route-src-bad", "route-src-bad@example.com");

        let reply = set_route(
            &state,
            "route-src-bad@example.com",
            &cond_route("", "auto", "nowhere", "deepseek-flash", None),
        );

        assert_eq!(answered(&reply), 400);
    }

    /// Two rules differing only in their conditions coexist.
    ///
    /// The end-to-end version of the `key` test: "all auto-mode traffic" and
    /// "Anthropic's auto-mode traffic" are different rules, and writing the
    /// second must not replace the first.
    #[test]
    fn rules_that_differ_only_in_conditions_coexist() {
        let state = state_with_deepseek("route-coexist", "route-coexist@example.com");
        let who = "route-coexist@example.com";

        set_route(
            &state,
            who,
            &cond_route("", "auto", "", "deepseek-flash", Some("deepseek")),
        )
        .expect("the general rule is valid");
        set_route(
            &state,
            who,
            &cond_route("", "auto", "anthropic", "deepseek-flash", Some("deepseek")),
        )
        .expect("the narrower rule is valid");

        let routes = routes_of(&state, who);
        assert_eq!(routes.len(), 2, "two rules, not one replacing the other");
        assert_eq!(routes[0].from_provider, None, "the general rule survives");
        assert_eq!(routes[1].from_provider.as_deref(), Some("anthropic"));
    }

    /// A model-only route still stores the kind as unset, which is what keeps it
    /// off the classifier.
    #[test]
    fn a_model_only_route_leaves_the_kind_unset() {
        let state = state_with_deepseek("route-modelonly", "route-modelonly@example.com");
        let who = "route-modelonly@example.com";

        set_route(&state, who, &route("claude-opus-5-5", "deepseek-flash", None)).expect("the route is valid");

        let routes = routes_of(&state, who);
        assert_eq!(routes[0].kind, None, "unset means work only");
        assert_eq!(routes[0].from.as_deref(), Some("claude-opus-5-5"));
    }

    // ── the account's declared kind ─────────────────────────────────────────

    /// Plan mode is reachable only by declaration, and it persists.
    ///
    /// This is the correction to a detector that was wrong: the plan tools are
    /// declared in EVERY Claude Code request, so `is_plan` matched all traffic and
    /// would have routed real work to whatever cheap model plan pointed at. Saying
    /// it per account is the only honest source, so the round trip through the
    /// store is the whole feature.
    #[test]
    fn plan_mode_is_reachable_by_declaration() {
        let state = state_for("kind-plan", "kind-plan@example.com");
        let who = "kind-plan@example.com";

        for spelling in ["plan", "plan_mode", "PLAN"] {
            let reply = set_kind(
                &state,
                who,
                &SetKind {
                    kind: spelling.to_owned(),
                },
            );
            assert_eq!(answered(&reply), 200, "{spelling} should be accepted");
        }

        // Read the value out and let the guard drop before asserting: holding a
        // lock across an assertion serves nothing and clippy rightly objects.
        let stored = {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.find(who).expect("the account").kind_override
        };
        assert_eq!(stored, Some(crate::classifier::Kind::Plan));
    }

    /// An empty kind clears it, returning the account to body classification.
    ///
    /// The undo, and it has to be a no-op rather than an error: an operator who
    /// clears an override that was not set is where they wanted to be.
    #[test]
    fn clearing_the_kind_override_is_not_an_error() {
        let state = state_for("kind-clear", "kind-clear@example.com");
        let who = "kind-clear@example.com";

        set_kind(
            &state,
            who,
            &SetKind {
                kind: "plan".to_owned(),
            },
        )
        .expect("set");
        let reply = set_kind(&state, who, &SetKind { kind: String::new() });

        assert_eq!(answered(&reply), 200);
        let stored = {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.find(who).expect("the account").kind_override
        };
        assert_eq!(stored, None);
    }

    /// An unknown kind is refused rather than silently treated as work.
    ///
    /// Guessing would be the dangerous direction here: a typo meant to declare
    /// plan mode would leave the account classified as ordinary writing traffic,
    /// which is the opposite of what the operator asked for — and invisible.
    #[test]
    fn an_unknown_kind_override_is_refused() {
        let state = state_for("kind-bad", "kind-bad@example.com");

        let reply = set_kind(
            &state,
            "kind-bad@example.com",
            &SetKind {
                kind: "planing".to_owned(),
            },
        );

        assert_eq!(answered(&reply), 400);
        let stored = {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.find("kind-bad@example.com").expect("the account").kind_override
        };
        assert_eq!(stored, None);
    }

    /// `plan` is accepted as a kind, and stored as such.
    ///
    /// Plan mode is the cheapest traffic to redirect — an investigation writes
    /// nothing — so it is the one an operator most wants a rule for.
    #[test]
    fn a_route_can_select_plan_mode() {
        let state = state_with_deepseek("route-plan", "route-plan@example.com");
        let who = "route-plan@example.com";

        for spelling in ["plan", "plan_mode"] {
            let reply = set_route(
                &state,
                who,
                &cond_route("", spelling, "", "deepseek-flash", Some("deepseek")),
            );
            assert_eq!(answered(&reply), 200, "{spelling} should be accepted");
        }

        let routes = routes_of(&state, who);
        assert_eq!(routes.len(), 1, "both spellings are the same rule");
        assert_eq!(routes[0].kind, Some(crate::classifier::Kind::Plan));
    }

    // ── the tool condition ──────────────────────────────────────────────────

    /// A route can select on the server-side tool a request declares.
    ///
    /// This is what lets search traffic be routed on its own: to a provider that
    /// runs it more cheaply, or to one that supports it at all, without moving
    /// the conversation that asked for it.
    #[test]
    fn a_route_can_select_on_a_server_tool() {
        let state = state_with_deepseek("route-tool", "route-tool@example.com");
        let who = "route-tool@example.com";

        let reply = set_route(&state, who, &tool_route("", "web_search", "deepseek-flash"));

        assert_eq!(answered(&reply), 200);
        let routes = routes_of(&state, who);
        assert_eq!(routes[0].tool.as_deref(), Some("web_search"));
        // A tool-only rule has no model condition: it catches search from any
        // model, which is the point of routing search separately.
        assert_eq!(routes[0].from, None);
        assert_eq!(routes[0].kind, None, "and it stays off the classifier");
    }

    /// A versioned tool type is normalised to its family.
    ///
    /// The type Anthropic sends carries a date (`web_search_20250305`). A rule
    /// stored with the date would stop applying when the version moves, so the
    /// family is what is kept — the same normalisation the usage badge relies on.
    #[test]
    fn a_versioned_tool_type_is_stored_as_its_family() {
        let state = state_with_deepseek("route-tool-ver", "route-tool-ver@example.com");
        let who = "route-tool-ver@example.com";

        set_route(&state, who, &tool_route("", "web_search_20260209", "deepseek-flash"))
            .expect("a versioned type is accepted");

        assert_eq!(routes_of(&state, who)[0].tool.as_deref(), Some("web_search"));
    }

    /// An unknown tool is refused rather than stored as a rule that never fires.
    #[test]
    fn an_unknown_tool_is_refused() {
        let state = state_with_deepseek("route-tool-bad", "route-tool-bad@example.com");

        let reply = set_route(
            &state,
            "route-tool-bad@example.com",
            &tool_route("", "web_crawl", "deepseek-flash"),
        );

        assert_eq!(answered(&reply), 400);
    }

    /// A tool rule and a model rule for the same model coexist: they select
    /// different traffic.
    #[test]
    fn a_tool_rule_does_not_replace_an_unrelated_rule() {
        let state = state_with_deepseek("route-tool-co", "route-tool-co@example.com");
        let who = "route-tool-co@example.com";

        set_route(&state, who, &route("claude-opus-5-5", "deepseek-flash", None)).expect("the model rule is valid");
        set_route(&state, who, &tool_route("", "web_search", "deepseek-flash")).expect("the tool rule is valid");

        assert_eq!(routes_of(&state, who).len(), 2);
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
