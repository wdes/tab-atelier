// SPDX-License-Identifier: MPL-2.0

//! Keys: minting one for an account, disabling it, revoking it.
//!
//! A key is the credential a tab presents. It belongs to exactly one account —
//! so every path here names the account as well as the key, and a key that
//! exists under a different account is a 404 rather than a silent success. That
//! check is why these functions take both names rather than a key id alone: the
//! store can then answer "does this key belong to this account" without either
//! caller having to remember to ask.

use std::sync::Arc;

use crate::http::requests::key::{AddKey, SetKeyDisabled};
use crate::http::resources;
use crate::server::State;
use crate::transport::{Reply, json_of};

/// An error as the API reports it, with the wording the store chose.
fn failure(status: u16, why: &str) -> Reply {
    crate::http::problem(status, why)
}

/// Mint a key for an account.
///
/// 201, not 200: something was created, and the caller has to read the body to
/// learn the secret. This is the only response that ever contains the key's
/// plaintext — the store keeps a hash — so a client that loses it must mint
/// another.
pub(crate) fn add(state: &Arc<State>, who: &str, req: &AddKey) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.add_key(who, &req.name) {
        Ok((key, secret)) => json_of(201, &resources::NewKeyResource::new(&key, secret)),
        // 400, not 404: the account exists and the request is what is wrong,
        // so a client should fix its body rather than its path.
        Err(e) => failure(400, &e.to_string()),
    }
}

/// Disable a key, or bring it back.
///
/// Disabling is the reversible half of revoking: the key stays in the account
/// and keeps its history, so a key turned off by mistake can be turned back on
/// without the owner having to reconfigure anything.
pub(crate) fn set_disabled(state: &Arc<State>, who: &str, key: &str, req: &SetKeyDisabled) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_key_disabled(who, key, req.disabled()) {
        Ok(k) => json_of(200, &resources::KeyEnvelope::from(&k)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Revoke a key.
///
/// Answers with what was removed, so the operator can confirm it was the key
/// they meant rather than one whose name they mistyped.
pub(crate) fn remove(state: &Arc<State>, who: &str, key: &str) -> Reply {
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.remove_key(who, key) {
        Ok(k) => json_of(200, &resources::RemovedKeyEnvelope::from(&k)),
        Err(e) => failure(404, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key name these tests use.
    ///
    /// Fixed rather than random because the store the tests share is on disk at
    /// `target/for-tests` and is never cleared, so a name has to mean the same
    /// thing on every run for the cleanup below to find it.
    const KEY: &str = "for-tests-laptop";

    /// An account of this test's own, on a store of its own.
    ///
    /// One account per test, named after the test, because the store these
    /// share is on disk and never cleared and the tests run in parallel: a
    /// common account would have them fighting over the same key names, and
    /// whichever lost would look like a failure in the code under test.
    fn state_with_account(email: &str) -> Arc<State> {
        let state = Arc::new(State::for_tests("t".to_owned()));
        {
            let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if store.find(email).is_none() {
                store.add("Ada", "Lovelace", email).expect("add");
            }
        }
        state
    }

    /// Mint a key, clearing any previous one of the same name first.
    ///
    /// Names are unique per account — that is how a key is revoked — so a test
    /// reusing a name has to clear it, and doing that here keeps the
    /// arrangement out of the assertions.
    fn mint(state: &Arc<State>, who: &str, name: &str) -> Reply {
        let _ = remove(state, who, name);
        add(state, who, &AddKey { name: name.to_owned() })
    }

    #[test]
    fn minting_a_key_answers_201_with_a_secret() {
        // 201 because something was created, and the body is the only place the
        // secret is ever readable — the store keeps a hash.
        let state = state_with_account("key-mint@example.com");
        let r = mint(&state, "key-mint@example.com", KEY);
        eprintln!("STATUS={} BODY={:?}", r.status, r.body);
        assert_eq!(r.status, 201);
    }

    #[test]
    fn minting_the_same_name_twice_is_refused() {
        // Names are how a key is revoked, so two on one account would make
        // "delete the laptop key" ambiguous.
        let state = state_with_account("key-dup@example.com");
        assert_eq!(mint(&state, "key-dup@example.com", KEY).status, 201);
        let again = add(&state, "key-dup@example.com", &AddKey { name: KEY.to_owned() });
        assert_ne!(again.status, 201, "a duplicate name must not mint a second key");
    }

    #[test]
    fn a_key_revoked_from_the_wrong_account_is_not_found() {
        // The account in the path is part of the key's identity. Answering
        // success here would let one account revoke another's credential by
        // guessing its name.
        let state = state_with_account("key-wrong-acct@example.com");
        assert_eq!(mint(&state, "key-wrong-acct@example.com", KEY).status, 201);
        assert_eq!(remove(&state, "nobody@example.com", KEY).status, 404);
    }

    #[test]
    fn revoking_a_key_answers_with_what_was_removed() {
        // The operator should be able to confirm it was the key they meant.
        let state = state_with_account("key-revoke@example.com");
        assert_eq!(mint(&state, "key-revoke@example.com", KEY).status, 201);
        let gone = remove(&state, "key-revoke@example.com", KEY);
        assert_eq!(gone.status, 200);
    }

    #[test]
    fn disabling_a_key_takes_effect_and_is_reversible() {
        // Disabling is the reversible half of revoking: a key turned off by
        // mistake can be turned back on without reconfiguring anyone.
        let state = state_with_account("key-disable@example.com");
        assert_eq!(mint(&state, "key-disable@example.com", KEY).status, 201);

        let off = set_disabled(
            &state,
            "key-disable@example.com",
            KEY,
            &SetKeyDisabled { disabled: Some(true) },
        );
        assert_eq!(off.status, 200);
        let on = set_disabled(
            &state,
            "key-disable@example.com",
            KEY,
            &SetKeyDisabled { disabled: Some(false) },
        );
        assert_eq!(on.status, 200);
    }
}
