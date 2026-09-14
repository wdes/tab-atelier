// SPDX-License-Identifier: MPL-2.0

//! Keys: minting one, disabling one, deleting one.
//!
//! All three live together because they share one rule that is easy to break
//! one endpoint at a time — **the secret is shown exactly once**, in the reply
//! to the mint, and never again from anywhere. Keeping the three in one file
//! makes that checkable by reading one file.

use std::sync::Arc;

use crate::http::requests::key::{AddKey, SetKeyDisabled};
use crate::http::resources;
use crate::server::State;
use crate::transport::{Reply, json_of};

/// The account and the key reference in `users/<who>/keys/<ref>`.
///
/// The two halves are separate because the store needs both: a key reference is
/// only unique within its owner, so `remove_key` cannot be asked with the
/// reference alone.
///
/// The reference may be empty — `users/<who>/keys` — which is what the mint
/// route passes, and which the store reads as "no particular key".
#[must_use]
fn split(rest: &str) -> (&str, &str) {
    let rest = rest.trim_start_matches("users/");
    // `.../keys` with nothing after it has no reference: the owner is
    // everything before the segment.
    rest.split_once("/keys/")
        .unwrap_or_else(|| (rest.strip_suffix("/keys").unwrap_or(rest), ""))
}

/// Mint a named key.
///
/// The name is what the operator will recognise in the table later, so an
/// unnamed request becomes `"new"` rather than an empty cell.
pub(crate) fn add(state: &Arc<State>, rest: &str, req: &AddKey) -> Reply {
    let (who, _) = split(rest);
    let name = if req.name.trim().is_empty() {
        "new"
    } else {
        req.name.trim()
    };
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.add_key(who, name) {
        Ok((k, secret)) => json_of(
            201,
            &resources::NewKeyResource {
                key: resources::KeyResource::from(&k),
                secret,
            },
        ),
        Err(e) => failure(400, &e.to_string()),
    }
}

/// Disable a key, or re-enable it.
///
/// Absent means disable: the caller that says nothing about the flag is a UI
/// posting to `/disabled`, which only exists for that one direction.
pub(crate) fn set_disabled(state: &Arc<State>, rest: &str, req: &SetKeyDisabled) -> Reply {
    let key_ref = rest.trim_end_matches("/disabled");
    let (who, key) = split(key_ref);
    let disabled = req.disabled.unwrap_or(true);
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.set_key_disabled(who, key, disabled) {
        Ok(k) => json_of(200, &resources::KeyEnvelope::from(&k)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// Delete a key. A deleted key stops working at once and cannot be recovered,
/// which is the point of having a separate verb from disabling it.
pub(crate) fn remove(state: &Arc<State>, rest: &str) -> Reply {
    let (who, key) = split(rest);
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match store.remove_key(who, key) {
        Ok(k) => json_of(200, &resources::RemovedKeyEnvelope::from(&k)),
        Err(e) => failure(404, &e.to_string()),
    }
}

/// An error as the API reports it, with the wording the store chose.
fn failure(status: u16, why: &str) -> Reply {
    crate::http::problem(status, why.to_owned())
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn a_key_path_splits_into_its_owner_and_its_reference() {
        assert_eq!(split("users/ada/keys/k1"), ("ada", "k1"));
        assert_eq!(split("users/ada/keys/k1/disabled"), ("ada", "k1/disabled"));
    }

    #[test]
    fn a_path_with_no_reference_has_an_empty_one() {
        assert_eq!(split("users/ada/keys"), ("ada", ""));
    }

    #[test]
    fn an_owner_named_keys_is_not_confused_with_the_keys_segment() {
        // `split_once` takes the FIRST occurrence, so the separator is the
        // literal "/keys/" and not the last slash-keys-slash in the string.
        assert_eq!(split("users/keys/keys/k1"), ("keys", "k1"));
    }
}
