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
        // The model list is rebuilt from text, so every field the text cannot
        // express is carried over by id — the same rule as the four lines above,
        // one level down. `price` is the one that shows: without it the row has
        // no recorded rate, and every hour it serves draws no money. It is also
        // the one the catalogue cannot always give back, because a rate set by
        // hand belongs to a model the catalogue has never heard of.
        //
        // `deprecated` and `note` ride along for the same reason the text has no
        // room for them. Only for a model the text still lists, though: one the
        // caller leaves out is dropped from the row, which the panel means to do
        // — it filters withdrawn models out of the list it sends.
        for model in &mut new.models {
            let Some(before) = old.models.iter().find(|m| m.id == model.id) else {
                continue;
            };
            if model.price.is_none() {
                model.price = before.price;
            }
            model.deprecated = before.deprecated;
            model.note.clone_from(&before.note);
        }
    } else if let Some(wanted) = request.enabled {
        new.enabled = wanted;
    }

    // Neither branch can supply a rate the form never had a field for. Take back
    // the one the catalogue publishes, which for a preset provider is where it
    // came from before the first save dropped it.
    new.adopt_published_rates();

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Auth, Class, Model, Preset};

    /// A state whose registry and key files live where this test can write.
    ///
    /// Per run as well as per test: the registry is written to disk, and a
    /// registry left behind would have the "provider does not exist yet"
    /// assertions passing or failing on what an earlier run happened to leave.
    fn state_for(name: &str) -> Arc<State> {
        let dir = std::env::temp_dir()
            .join("tab-atelier-provider-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut state = State::for_tests("t".to_owned());
        state.registry_path = dir.join("providers.json");
        Arc::new(state)
    }

    /// A save of a hand-configured provider.
    fn custom(id: &str) -> SaveProvider {
        SaveProvider {
            preset: String::new(),
            id: id.to_owned(),
            base_url: "https://example.invalid".to_owned(),
            models: "model-a:fast\nmodel-b:balanced".to_owned(),
            key: String::new(),
            preference: None,
            dup: None,
            enabled: None,
        }
    }

    /// Put a provider into the registry directly, bypassing the form.
    ///
    /// The form has no field for the auth kind or the rotation weight, so the
    /// only way to set up the state those tests are about is behind its back —
    /// which is exactly the situation being checked.
    fn seed(state: &Arc<State>, provider: crate::provider::Provider) {
        state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upsert(provider);
    }

    /// A provider of the kind only the registry can hold: no key file, the
    /// host's own login.
    fn host_login(state: &Arc<State>, id: &str, weight: i32) -> crate::provider::Provider {
        let mut p = Preset::Deepseek.provider(&registry_dir(state));
        p.id = id.to_owned();
        p.auth = Auth::ClaudeOauth;
        p.preference = weight;
        p
    }

    fn ids(state: &Arc<State>) -> Vec<String> {
        state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect()
    }

    // ── saving ──────────────────────────────────────────────────────────────

    #[test]
    fn a_hand_configured_provider_is_saved_and_named_back() {
        let state = state_for("custom");
        let reply = save(&custom("my-hop"), &state);
        assert_eq!(reply.status, 200);
        assert!(ids(&state).contains(&"my-hop".to_owned()));
    }

    /// A trailing newline from a pasted list does not fail the form.
    ///
    /// Blank lines are skipped rather than refused. Refusing would mean that
    /// copying a list out of a document — which almost always ends in a
    /// newline — returns "no models listed" for what looks like a perfectly
    /// good list.
    #[test]
    fn blank_lines_in_a_pasted_model_list_are_skipped() {
        let state = state_for("blank-model");
        let mut req = custom("blank-hop");
        req.models = "model-a:fast\n\n  \nmodel-b:balanced\n".to_owned();
        assert_eq!(save(&req, &state).status, 200);

        let models = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("blank-hop")
            .expect("saved")
            .models
            .clone();
        assert_eq!(models.len(), 2, "two models, with the blanks dropped: {models:?}");
    }

    /// A list that parses to nothing IS refused.
    ///
    /// The other half of the rule above: skipping blanks must not extend to
    /// accepting a list that has nothing in it at all, because a provider with
    /// no models would be offered and every request for it would fail.
    #[test]
    fn a_model_list_with_nothing_in_it_is_refused() {
        let state = state_for("no-models");
        let mut req = custom("empty-hop");
        req.models = "\n  \n".to_owned();
        assert_eq!(save(&req, &state).status, 400);

        let mut blank = custom("empty-hop");
        blank.models = String::new();
        assert_eq!(save(&blank, &state).status, 400, "and so is an absent list");
    }

    /// A class that is not one of the three is refused, by name.
    ///
    /// The classes decide which hop a request can go to, so an unrecognised one
    /// silently defaulting would put every request for that model through a lane
    /// nobody chose.
    #[test]
    fn a_model_class_nobody_defines_is_refused() {
        let state = state_for("bad-class");
        let mut req = custom("bad-class-hop");
        req.models = "model-a:sprightly".to_owned();
        assert_eq!(save(&req, &state).status, 400);

        let mut missing = custom("bad-class-hop");
        missing.models = "model-a".to_owned();
        assert_eq!(save(&missing, &state).status, 400, "the class is not optional");
    }

    /// The cost is optional and defaults to one.
    ///
    /// A model of a given class is priced relative to its siblings, and leaving
    /// the number off must mean "the baseline", not "free" or "unknown".
    #[test]
    fn a_model_without_a_cost_is_priced_at_the_baseline() {
        let state = state_for("cost-default");
        let mut req = custom("cost-hop");
        req.models = "model-a:fast".to_owned();
        assert_eq!(save(&req, &state).status, 200);

        let cost = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("cost-hop")
            .expect("saved")
            .models[0]
            .relative_cost;
        assert_eq!(cost, 1);
    }

    #[test]
    fn a_cost_that_is_not_a_whole_number_is_refused() {
        // It is multiplied into a price, so a fractional or negative value would
        // propagate a nonsense number rather than fail anywhere visible.
        let state = state_for("bad-cost");
        let mut req = custom("bad-cost-hop");
        req.models = "model-a:fast:half".to_owned();
        assert_eq!(save(&req, &state).status, 400);
    }

    #[test]
    fn a_save_keeps_the_authentication_kind_it_did_not_come_with() {
        // The form has no field for the auth kind, so a save used to rewrite it
        // from whatever the request implied. For the subscription's own row that
        // meant `claude_oauth` became `api_key_file`, which left it with no
        // credential at all, out of the candidate list, and the proxy falling
        // back to nothing. A partial view must save partially.
        let state = state_for("preserve-auth");
        assert_eq!(save(&custom("shared"), &state).status, 200);

        seed(&state, host_login(&state, "shared", 10));

        // A second save through the form, carrying a key the way the UI would.
        let mut again = custom("shared");
        again.key = "sk-ant-something".to_owned();
        assert_eq!(save(&again, &state).status, 200);

        let kept = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("shared")
            .expect("still there")
            .auth
            .clone();
        assert!(
            matches!(kept, Auth::ClaudeOauth),
            "the auth kind was overwritten by a form that has no field for it: {kept:?}"
        );
    }

    #[test]
    fn a_save_keeps_the_rotation_weight_the_form_cannot_express() {
        // Same reasoning as the auth kind: the weight is not on the form, and
        // silently resetting it to the default would change how traffic is
        // distributed across providers by editing something else.
        let state = state_for("preserve-weight");
        seed(&state, host_login(&state, "weighted", 42));
        assert_eq!(save(&custom("weighted"), &state).status, 200);

        let weight = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("weighted")
            .expect("still there")
            .preference;
        assert_eq!(weight, 42, "the weight survived a save that cannot mention it");
    }

    /// The rate is not on the form either, and a save used to drop it.
    ///
    /// The model list round-trips as `id:class:cost` text — the shape the panel
    /// builds at `web/src/app.ts` and this crate parses back — and a rate has no
    /// place in it. So the one field that decides whether an hour can draw a
    /// money figure was deleted by any save at all, including the ones that have
    /// nothing to do with models: `setCompact`, the dropdown that changes how
    /// much history a provider is sent, sends the whole list back as text.
    ///
    /// The symptom is silence, which is why it needs a test rather than a look:
    /// the hop keeps serving, keeps counting tokens, and the money unit stays
    /// empty for ever.
    ///
    /// Priced by hand on a model the catalogue does not know, on purpose. The
    /// catalogue can only hand back what it publishes, so this is the one case
    /// where carrying the old value across is the only thing standing between an
    /// operator's own figure and the repair that cannot replace it.
    #[test]
    fn a_save_keeps_the_price_the_form_cannot_express() {
        let state = state_for("preserve-price");
        let mut mine = host_login(&state, "mine", 10);
        mine.models = vec![Model::new("bespoke-model", Class::Balanced, 7).priced(1, 2, 3)];
        let by_hand = mine.models[0].price;
        assert!(by_hand.is_some(), "the fixture has a rate to lose");
        assert!(
            provider::published_price("bespoke-model").is_none(),
            "the model must be one the catalogue cannot re-supply"
        );
        seed(&state, mine);

        // A save exactly as the panel sends one: every model it lists, in text.
        let mut req = custom("mine");
        req.models = "bespoke-model:balanced:7".to_owned();
        assert_eq!(save(&req, &state).status, 200);

        let price = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("mine")
            .and_then(|p| p.models.iter().find(|m| m.id == "bespoke-model"))
            .expect("the model is still there")
            .price;
        assert_eq!(
            price, by_hand,
            "the rate was overwritten by a form that has no field for it, \
             and the catalogue has none to give back"
        );
    }

    /// A row that has already lost its rates gets them back on the next save.
    ///
    /// Carrying the old ones over only helps a row that still has them. A file
    /// written before the field existed deserialises without them, and so does a
    /// row every earlier save stripped — for those the catalogue has to be the
    /// source, or they are unpriced for ever.
    #[test]
    fn a_save_returns_the_rate_the_catalogue_publishes() {
        let state = state_for("restore-price");
        let mut bare = Preset::Deepseek.provider(&registry_dir(&state));
        for model in &mut bare.models {
            model.price = None;
        }
        seed(&state, bare);

        let mut req = custom("deepseek");
        req.base_url = "https://api.deepseek.com/anthropic".to_owned();
        req.models = "deepseek-flash:balanced:15".to_owned();
        assert_eq!(save(&req, &state).status, 200);

        let price = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("deepseek")
            .and_then(|p| p.models.iter().find(|m| m.id == "deepseek-flash"))
            .expect("the model is still there")
            .price;
        assert!(
            price.is_some(),
            "a row that lost its rate must take the published one back"
        );
    }

    #[test]
    fn a_save_that_names_enabled_is_taken_at_its_word() {
        // The table DOES show this one, so an explicit value wins.
        let state = state_for("enabled-explicit");
        assert_eq!(save(&custom("toggled"), &state).status, 200);

        let mut off = custom("toggled");
        off.enabled = Some(false);
        assert_eq!(save(&off, &state).status, 200);

        let enabled = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("toggled")
            .expect("still there")
            .enabled;
        assert!(!enabled, "the request asked for off");
    }

    #[test]
    fn a_save_that_does_not_mention_enabled_keeps_the_previous_answer() {
        // This is what keeps the older form working: a client that predates the
        // field must not silently re-enable a provider the operator turned off.
        let state = state_for("enabled-preserved");
        assert_eq!(save(&custom("kept-off"), &state).status, 200);
        {
            let mut off = custom("kept-off");
            off.enabled = Some(false);
            assert_eq!(save(&off, &state).status, 200);
        }
        // Now a save with no opinion at all.
        assert_eq!(save(&custom("kept-off"), &state).status, 200);

        let enabled = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("kept-off")
            .expect("still there")
            .enabled;
        assert!(!enabled, "a save with no opinion must not re-enable");
    }

    // ── presets and copies ──────────────────────────────────────────────────

    #[test]
    fn a_preset_save_adds_the_provider_it_names() {
        let state = state_for("preset");
        let mut req = custom("ignored");
        req.preset = Preset::Deepseek.id().to_owned();
        assert_eq!(save(&req, &state).status, 200);
        assert!(ids(&state).contains(&Preset::Deepseek.id().to_owned()));
    }

    #[test]
    fn a_copy_of_a_preset_gets_a_name_of_its_own() {
        // The whole point of `dup`: a second credential for the same provider.
        // Without a distinct id the two would share one key file, and the key
        // pasted last would land on top of the one before it — leaving the
        // operator holding one credential while believing they had two.
        let state = state_for("dup");
        let mut first = custom("ignored");
        first.preset = Preset::Deepseek.id().to_owned();
        assert_eq!(save(&first, &state).status, 200);

        let mut second = custom("ignored");
        second.preset = Preset::Deepseek.id().to_owned();
        second.dup = Some(true);
        second.key = "a-second-key".to_owned();
        assert_eq!(save(&second, &state).status, 200);

        let all = ids(&state);
        let copies = all.iter().filter(|id| *id != "openai").count();
        assert!(copies >= 2, "the copy got its own row: {all:?}");
        assert_eq!(
            all.iter().filter(|id| id.starts_with(Preset::Deepseek.id())).count(),
            2,
            "two deepseek rows, distinguishable: {all:?}"
        );
    }

    // ── rotating a key ──────────────────────────────────────────────────────

    #[test]
    fn rotating_the_key_of_a_provider_that_is_not_there_is_a_404() {
        let state = state_for("rotate-missing");
        let reply = rotate_key("nobody", &RotateProviderKey { key: "k".to_owned() }, &state);
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn rotating_a_key_on_a_provider_that_cannot_hold_one_is_refused() {
        // Writing a key for a `claude_oauth` provider used to succeed and do
        // nothing — the file is never read, because the egress uses the host's
        // own login. It left a live credential on disk that no code path
        // consults, and an operator who later changed the auth kind would find
        // a key they had forgotten they pasted, suddenly in use.
        let state = state_for("rotate-oauth");
        seed(&state, host_login(&state, "host-login", 10));

        let reply = rotate_key(
            "host-login",
            &RotateProviderKey {
                key: "sk-never-read".to_owned(),
            },
            &state,
        );
        assert_eq!(reply.status, 400, "refused rather than silently ignored");

        // And nothing was written: the point of refusing is that no credential
        // is left on disk.
        let path = provider::provider_key_path(&registry_dir(&state), "host-login");
        assert!(!path.exists(), "a key was written anyway: {}", path.display());
    }

    // ── removing ────────────────────────────────────────────────────────────

    #[test]
    fn removing_a_provider_that_is_not_there_is_a_404() {
        let state = state_for("remove-missing");
        assert_eq!(remove("nobody", &state).status, 404);
    }

    #[test]
    fn removing_a_provider_unpins_whoever_was_routed_to_it() {
        // The unpinning is not optional. A pin is a person saying "send me
        // there"; leaving one behind after the destination is gone is how a
        // working account becomes a 502 that no page explains.
        let state = state_for("unpin");
        assert_eq!(save(&custom("doomed"), &state).status, 200);

        {
            let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.add("Ada", "Lovelace", "unpin@example.com").expect("add");
            store.set_provider("unpin@example.com", Some("doomed")).expect("pin");
        }

        assert_eq!(remove("doomed", &state).status, 200);

        let pinned = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("unpin@example.com")
            .expect("the account is still there")
            .provider
            .clone();
        assert!(
            pinned.is_none(),
            "the account is still routed at a provider that no longer exists: {pinned:?}"
        );
    }

    #[test]
    fn removing_a_provider_leaves_an_account_pinned_elsewhere_alone() {
        // Only the accounts routed at the one being removed are touched;
        // unpinning everybody would silently undo somebody else's choice.
        let state = state_for("unpin-others");
        assert_eq!(save(&custom("doomed"), &state).status, 200);
        assert_eq!(save(&custom("survivor"), &state).status, 200);
        {
            let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            store.add("Ada", "Lovelace", "elsewhere@example.com").expect("add");
            store
                .set_provider("elsewhere@example.com", Some("survivor"))
                .expect("pin");
        }

        assert_eq!(remove("doomed", &state).status, 200);

        let pinned = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("elsewhere@example.com")
            .expect("the account")
            .provider
            .clone();
        assert_eq!(pinned.as_deref(), Some("survivor"), "untouched");
    }
}
