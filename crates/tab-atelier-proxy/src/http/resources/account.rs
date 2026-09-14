// SPDX-License-Identifier: MPL-2.0

//! Accounts and keys, as the API presents them.
//!
//! Every field is owned rather than borrowed. A borrowed resource could only be
//! serialized while the account store's lock was still held, which turns a
//! response shape into a lock-ordering constraint; the copies are a few short
//! strings on an admin route and buy a resource that outlives its borrow.

use serde::Serialize;

use crate::users;

/// One key, without its hash.
///
/// The hash is not a usable secret, but it is the input to an offline guess,
/// and the UI has no reason to hold it.
#[derive(Serialize)]
pub(crate) struct KeyResource {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub first_used_at: Option<u64>,
    pub last_used_at: Option<u64>,
    pub last_used_ip: Option<String>,
    pub disabled: bool,
}

impl From<&users::Key> for KeyResource {
    fn from(k: &users::Key) -> Self {
        Self {
            id: k.id.clone(),
            name: k.name.clone(),
            created_at: k.created_at,
            first_used_at: k.first_used_at,
            last_used_at: k.last_used_at,
            last_used_ip: k.last_used_ip.clone(),
            disabled: k.disabled,
        }
    }
}

/// A person, with every key they hold.
#[derive(Serialize)]
pub(crate) struct AccountResource {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub created_at: u64,
    pub disabled: bool,
    pub weight: u32,
    /// The pin, so a row can say where this person's work goes.
    pub provider: Option<String>,
    /// The model pin, which outranks the provider pin: choosing a model
    /// chooses the hop that serves it. This is also what makes a model
    /// selectable per person, since routing resolves the id to a provider.
    pub model: Option<String>,
    /// This person's compaction level. Per ACCOUNT, not per provider — the two
    /// axes meet in the account, and the level is what the relay reads.
    pub compact: &'static str,
    /// The whole policy, not a summary of it: the UI edits it field by field,
    /// so it needs the parts it is not currently changing.
    pub tools: crate::tools::Policy,
    /// Every key, each with its own history.
    pub keys: Vec<KeyResource>,
    /// The account's most recent activity across all its keys, for the row
    /// summary.
    pub last_used_at: Option<u64>,
    pub has_key: bool,
}

impl From<&users::Account> for AccountResource {
    fn from(a: &users::Account) -> Self {
        Self {
            id: a.id.clone(),
            first_name: a.first_name.clone(),
            last_name: a.last_name.clone(),
            email: a.email.clone(),
            created_at: a.created_at,
            disabled: a.disabled,
            weight: a.weight,
            provider: a.provider.clone(),
            model: a.model.clone(),
            compact: a.compact.as_str(),
            tools: a.tools.clone(),
            keys: a.keys.iter().map(KeyResource::from).collect(),
            last_used_at: a.keys.iter().filter_map(|k| k.last_used_at).max(),
            has_key: a.keys.iter().any(users::Key::active),
        }
    }
}

/// `{"user": …}`, which is how every single-account route answers.
///
/// The envelope is a struct rather than a `json!` wrapper so the response shape
/// is one definition: the handler serializes it, okapi reads it, and neither
/// can drift from the other.
#[derive(Serialize)]
pub(crate) struct AccountEnvelope {
    pub user: AccountResource,
}

impl From<&users::Account> for AccountEnvelope {
    fn from(a: &users::Account) -> Self {
        Self {
            user: AccountResource::from(a),
        }
    }
}

/// `{"removed": …}`, for a deletion that reports what it removed.
#[derive(Serialize)]
pub(crate) struct RemovedAccountEnvelope {
    pub removed: AccountResource,
}

impl From<&users::Account> for RemovedAccountEnvelope {
    fn from(a: &users::Account) -> Self {
        Self {
            removed: AccountResource::from(a),
        }
    }
}

/// `{"users": […]}`, for the list route.
#[derive(Serialize)]
pub(crate) struct AccountsResource {
    pub users: Vec<AccountResource>,
}

impl AccountsResource {
    #[must_use]
    pub fn of(accounts: &[users::Account]) -> Self {
        Self {
            users: accounts.iter().map(AccountResource::from).collect(),
        }
    }
}

/// `{"key": …}`, which is how a key is reported.
#[derive(Serialize)]
pub(crate) struct KeyEnvelope {
    pub key: KeyResource,
}

impl From<&users::Key> for KeyEnvelope {
    fn from(k: &users::Key) -> Self {
        Self {
            key: KeyResource::from(k),
        }
    }
}

/// `{"removed": …}`, for a key deletion.
#[derive(Serialize)]
pub(crate) struct RemovedKeyEnvelope {
    pub removed: KeyResource,
}

impl From<&users::Key> for RemovedKeyEnvelope {
    fn from(k: &users::Key) -> Self {
        Self {
            removed: KeyResource::from(k),
        }
    }
}

/// A newly minted key: the only moment the secret exists in the clear.
///
/// `secret` is a separate field from the [`KeyResource`], not a property of it,
/// because a key never carries its own secret afterwards — keeping them in one
/// struct would mean every other response had to remember to omit it.
#[derive(Serialize)]
pub(crate) struct NewKeyResource {
    pub key: KeyResource,
    pub secret: String,
}
