// SPDX-License-Identifier: MPL-2.0

//! The user key guard: who is this, and record that they were here.
//!
//! This is the middleware most requests pass through, and the one that writes.
//! Stamping on every authenticated request is the point — "who has this key
//! been used by" is only answerable if the answer is kept current.

use crate::server::State;
use crate::users::Account;

/// The account behind a key, and the id of that key, stamped with this
/// sighting.
///
/// The key id is returned alongside the account because the sighting describes
/// the KEY, and the session a request arrived from is part of it — the caller
/// that records that session needs to know which key's history to extend.
///
/// Returns `None` for an unknown, disabled or empty key — the caller decides
/// how to say so, because the relay and the personal routes phrase a refusal
/// differently.
#[must_use]
pub fn authenticate_and_stamp(state: &State, key: &str, ip: &str) -> Option<(Account, String)> {
    // The KEY is stamped, not the account: "last used from here" says nothing
    // when several keys share an account, which is the whole reason they are
    // named separately.
    //
    // The session is NOT stamped here. This runs as a guard, before the body
    // has been read, and the session lives in the body. The relay adds it once
    // it has one, which is also why `touch` takes an `Option` client: this call
    // and that one describe the same sighting, one field at a time.
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let found = store.authenticate_key(key).map(|(a, k)| (a.clone(), k.id.clone()));
    if let Some((_, key_id)) = &found {
        store.touch(key_id, Some(ip), None);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::Store;

    /// A store of this test's own, with one account and one key.
    ///
    /// Named after the test, because the store writes to disk: a shared path
    /// would have the tests reading each other's keys, and "unknown key" would
    /// stop meaning what the test thinks it means.
    fn store(name: &str) -> (Store, String) {
        // Per run as well as per test: a key's plaintext is returned once, at
        // mint time, so a store left over from a previous run cannot be re-read
        // — the secret is unrecoverable and the test would have nothing to
        // authenticate with.
        let dir = std::env::temp_dir()
            .join("tab-atelier-user-key-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut store = Store::load(dir.join("users.json")).expect("a fresh store");
        store
            .add("Ada", "Lovelace", &format!("{name}@example.com"))
            .expect("add");
        let (_, secret) = store.add_key(&format!("{name}@example.com"), "laptop").expect("mint");
        (store, secret)
    }

    fn state_with(store: Store) -> State {
        let state = State::for_tests("t".to_owned());
        *state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = store;
        state
    }

    #[test]
    fn a_minted_key_authenticates_to_its_account() {
        let (store, secret) = store("valid");
        let state = state_with(store);
        let (account, key_id) = authenticate_and_stamp(&state, &secret, "203.0.113.7").expect("authenticates");
        assert_eq!(account.email, "valid@example.com");
        // The key id has to come back with the account: the relay stamps the
        // session that made the request onto this key, and an id lost here
        // would attribute every tab to whichever key happened to be looked up.
        assert!(!key_id.is_empty(), "the key id identifies which key it was");
    }

    #[test]
    fn an_unknown_key_resolves_to_nobody() {
        // The one case that has to be airtight: a key the store does not know
        // must not be attributed to an account, or a guessed secret would relay
        // on someone else's usage.
        let (store, _) = store("unknown");
        let state = state_with(store);
        assert!(authenticate_and_stamp(&state, "not-a-real-key", "203.0.113.7").is_none());
    }

    #[test]
    fn an_empty_key_resolves_to_nobody() {
        // A request with no credential at all is the common case on a
        // misconfigured client, and it must not match an account with no keys.
        let (store, _) = store("empty");
        let state = state_with(store);
        assert!(authenticate_and_stamp(&state, "", "203.0.113.7").is_none());
    }

    #[test]
    fn a_disabled_key_stops_authenticating() {
        // Disabling is the operator's off switch, and it has to work on the next
        // request rather than the next restart.
        let (store, secret) = store("disabled");
        let state = state_with(store);
        assert!(authenticate_and_stamp(&state, &secret, "203.0.113.7").is_some());

        state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_key_disabled("disabled@example.com", "laptop", true)
            .expect("disable");
        assert!(authenticate_and_stamp(&state, &secret, "203.0.113.7").is_none());
    }

    #[test]
    fn a_revoked_key_stops_authenticating() {
        let (store, secret) = store("revoked");
        let state = state_with(store);
        state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_key("revoked@example.com", "laptop")
            .expect("revoke");
        assert!(authenticate_and_stamp(&state, &secret, "203.0.113.7").is_none());
    }

    #[test]
    fn authenticating_records_where_the_key_was_used_from() {
        // This is the whole reason the function stamps rather than only looking
        // up: "which address used this key last" is only answerable if every
        // use updates it.
        let (store, secret) = store("stamps");
        let state = state_with(store);
        authenticate_and_stamp(&state, &secret, "203.0.113.7").expect("authenticates");

        // The key is copied out, so the guard's last use is the copy and the
        // lock is released there rather than at the end of the test.
        let key = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("stamps@example.com")
            .expect("the account")
            .keys[0]
            .clone();
        let (ip, at) = (key.last_used_ip, key.last_used_at);
        assert_eq!(ip.as_deref(), Some("203.0.113.7"), "the address is kept");
        assert!(at.is_some(), "and the moment");
    }

    #[test]
    fn a_refused_key_records_nothing() {
        // A failed attempt must not update the account, or a flood of guesses
        // would rewrite the real owner's last-seen address.
        let (store, _) = store("no-stamp");
        let state = state_with(store);
        assert!(authenticate_and_stamp(&state, "guess", "198.51.100.9").is_none());

        let ip = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("no-stamp@example.com")
            .expect("the account")
            .keys[0]
            .last_used_ip
            .clone();
        assert!(ip.is_none(), "the account was left alone");
    }

    #[test]
    fn the_stamp_lands_on_the_key_used_not_on_a_sibling() {
        // Deliberate: an account with a laptop and a phone would otherwise be
        // unable to say which one was last seen where, which is exactly the
        // question an operator investigating a leak is asking.
        let (store, secret) = store("sibling");
        let state = state_with(store);
        {
            let mut s = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.add_key("sibling@example.com", "phone").expect("mint");
        }
        authenticate_and_stamp(&state, &secret, "203.0.113.7").expect("authenticates");

        let keys = state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .find("sibling@example.com")
            .expect("the account")
            .keys
            .clone();
        let of = |name: &str| {
            keys.iter()
                .find(|k| k.name == name)
                .unwrap_or_else(|| panic!("{name} is on the account"))
                .last_used_ip
                .clone()
        };
        let (laptop, phone) = (of("laptop"), of("phone"));
        assert_eq!(laptop.as_deref(), Some("203.0.113.7"));
        assert!(phone.is_none(), "the other key is untouched");
    }

    #[test]
    fn the_same_key_authenticates_repeatedly() {
        // Stamping must not consume or rotate anything: this runs on every
        // request a tab makes.
        let (store, secret) = store("repeat");
        let state = state_with(store);
        for _ in 0..3 {
            assert!(authenticate_and_stamp(&state, &secret, "203.0.113.7").is_some());
        }
    }
}
