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

impl NewKeyResource {
    /// The one response that carries a key's plaintext.
    ///
    /// The store keeps only a hash, so this is the only moment the secret is
    /// readable — a client that loses it has to mint another, and the doc
    /// comment on the route is where that is said out loud.
    #[must_use]
    pub(crate) fn new(key: &users::Key, secret: String) -> Self {
        Self {
            key: KeyResource::from(key),
            secret,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::Store;

    /// A store of this test's own, holding one account.
    ///
    /// The path is named after the account, so no two tests in this module
    /// share a file. They would otherwise inherit each other's accounts and
    /// fail on a name collision that has nothing to do with what is being
    /// checked.
    /// Per run as well as per account: a key's plaintext is only readable once,
    /// at mint time, so a store left behind by a previous run cannot be reused —
    /// the secret is unrecoverable and the test would have nothing to check
    /// against.
    fn store_with(email: &str) -> Store {
        let dir = std::env::temp_dir()
            .join("tab-atelier-res-tests")
            .join(format!("{email}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut store = Store::load(dir.join("users.json")).expect("fresh store");
        store.add("Ada", "Lovelace", email).expect("add");
        store
    }

    /// The account, copied out so a test can set a field the store owns.
    fn account_of(store: &Store, email: &str) -> users::Account {
        store.find(email).cloned().expect("the account was added")
    }

    #[test]
    fn an_account_with_no_keys_says_so() {
        // The flag drives a warning in the UI, so a wrong `true` hides an
        // account nobody can use.
        let store = store_with("res-no-keys@example.com");
        let r = AccountResource::from(&account_of(&store, "res-no-keys@example.com"));
        assert!(!r.has_key);
        assert!(r.keys.is_empty());
        assert!(r.last_used_at.is_none());
    }

    #[test]
    fn an_account_with_one_usable_key_says_so() {
        let mut store = store_with("res-one-key@example.com");
        store.add_key("res-one-key@example.com", "laptop").expect("mint");
        let r = AccountResource::from(&account_of(&store, "res-one-key@example.com"));
        assert!(r.has_key);
        assert_eq!(r.keys.len(), 1);
        assert_eq!(r.keys[0].name, "laptop");
    }

    #[test]
    fn disabling_every_key_clears_the_flag_and_leaves_the_keys_visible() {
        // This is the whole reason the flag is separate from the list: an
        // account whose only key is off has keys, and cannot be used. A UI that
        // read `keys.is_empty()` would show it as merely new.
        let mut store = store_with("res-off-key@example.com");
        store.add_key("res-off-key@example.com", "laptop").expect("mint");
        store
            .set_key_disabled("res-off-key@example.com", "laptop", true)
            .expect("disable");

        let r = AccountResource::from(&account_of(&store, "res-off-key@example.com"));
        assert_eq!(r.keys.len(), 1, "the key is still listed");
        assert!(r.keys[0].disabled, "and is marked disabled");
        assert!(!r.has_key, "but the account has nothing usable");
    }

    #[test]
    fn the_last_use_reported_is_the_most_recent_of_all_the_keys() {
        // Not the first key's, and not the last in the list: the panel reads this
        // as "when was this account last active", and a laptop that has not been
        // opened for a month must not hide a phone used this morning.
        let mut store = store_with("res-last-use@example.com");
        store.add_key("res-last-use@example.com", "laptop").expect("mint");
        store.add_key("res-last-use@example.com", "phone").expect("mint");

        let mut account = account_of(&store, "res-last-use@example.com");
        account.keys[0].last_used_at = Some(1_000);
        account.keys[1].last_used_at = Some(9_000);
        let r = AccountResource::from(&account);
        assert_eq!(r.last_used_at, Some(9_000));
    }

    #[test]
    fn a_key_never_used_does_not_count_as_a_use() {
        // `None` rather than zero: a zero would render as 1970, which reads as
        // "used once, long ago" rather than "never".
        let mut store = store_with("res-never-used@example.com");
        store.add_key("res-never-used@example.com", "laptop").expect("mint");
        let mut account = account_of(&store, "res-never-used@example.com");
        account.keys[0].last_used_at = None;
        account.keys[0].first_used_at = None;
        let r = AccountResource::from(&account);
        assert!(r.last_used_at.is_none());
        assert!(r.keys[0].first_used_at.is_none());
    }

    #[test]
    fn an_override_the_operator_has_not_set_is_absent_rather_than_empty() {
        // The UI shows the installation default when these are absent. An empty
        // string would instead render as a blank field the operator thinks they
        // set.
        let store = store_with("res-no-override@example.com");
        let r = AccountResource::from(&account_of(&store, "res-no-override@example.com"));
        assert!(r.provider.is_none());
        assert!(r.model.is_none());
    }

    #[test]
    fn an_override_the_operator_has_set_survives_into_the_response() {
        let mut store = store_with("res-override@example.com");
        store
            .set_provider("res-override@example.com", Some("deepseek"))
            .expect("set");
        let r = AccountResource::from(&account_of(&store, "res-override@example.com"));
        assert_eq!(r.provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn the_compaction_level_is_sent_as_its_token() {
        // The token is the wire contract the UI and the store both read; the
        // label is a language and lives in the markup.
        let store = store_with("res-compact@example.com");
        let account = account_of(&store, "res-compact@example.com");
        let r = AccountResource::from(&account);
        assert_eq!(r.compact, account.compact.as_str());
    }

    #[test]
    fn a_collection_keeps_the_order_it_was_given() {
        // The dashboard renders these in order, and `Store::accounts` is
        // ordered by when they were added. Reordering would make the list jump
        // between requests.
        let mut store = store_with("res-order-1@example.com");
        store.add("Ada", "L2", "res-order-2@example.com").expect("add");
        let all = store.accounts().to_vec();
        let resource = AccountsResource::of(&all);
        assert_eq!(resource.users.len(), 2);
        assert_eq!(resource.users[0].email, "res-order-1@example.com");
        assert_eq!(resource.users[1].email, "res-order-2@example.com");
    }

    #[test]
    fn the_envelope_key_names_are_the_wire_contract() {
        // These strings are what the generated TypeScript reads. `user` and not
        // `account`, here and in every single-account response.
        let store = store_with("res-envelope@example.com");
        let account = account_of(&store, "res-envelope@example.com");

        let one = serde_json::to_string(&AccountEnvelope::from(&account)).expect("serialize");
        assert!(one.starts_with("{\"user\":{"), "{one}");

        let removed = serde_json::to_string(&RemovedAccountEnvelope::from(&account)).expect("serialize");
        assert!(removed.starts_with("{\"removed\":{"), "{removed}");

        let many = serde_json::to_string(&AccountsResource::of(&[account])).expect("serialize");
        assert!(many.starts_with("{\"users\":["), "{many}");
    }

    #[test]
    fn a_key_is_reported_under_its_own_names_too() {
        let mut store = store_with("res-key-envelope@example.com");
        let (key, _secret) = store.add_key("res-key-envelope@example.com", "laptop").expect("mint");

        let one = serde_json::to_string(&KeyEnvelope::from(&key)).expect("serialize");
        assert!(one.starts_with("{\"key\":{"), "{one}");

        let removed = serde_json::to_string(&RemovedKeyEnvelope::from(&key)).expect("serialize");
        assert!(removed.starts_with("{\"removed\":{"), "{removed}");
    }

    #[test]
    fn the_secret_appears_only_in_the_response_that_created_the_key() {
        // The store keeps a hash, so this is the only moment the plaintext
        // exists. If it ever leaked into the account listing, every read of the
        // account list would hand out working credentials.
        let mut store = store_with("res-secret@example.com");
        let (key, secret) = store.add_key("res-secret@example.com", "laptop").expect("mint");
        assert!(!secret.is_empty(), "the store returned a secret");

        let created = serde_json::to_string(&NewKeyResource::new(&key, secret.clone())).expect("serialize");
        assert!(created.contains(&secret), "the creation response carries it");

        let account = account_of(&store, "res-secret@example.com");
        let listed = serde_json::to_string(&AccountsResource::of(&[account])).expect("serialize");
        assert!(
            !listed.contains(&secret),
            "listing the accounts must never hand out a key"
        );
    }

    #[test]
    fn the_key_created_response_puts_the_key_beside_its_secret() {
        // The UI reads `key.name` for the label and `secret` for the value, so
        // they have to be siblings rather than nested.
        let mut store = store_with("res-key-created@example.com");
        let (key, secret) = store.add_key("res-key-created@example.com", "laptop").expect("mint");
        let body = serde_json::to_string(&NewKeyResource::new(&key, secret.clone())).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["key"]["name"], "laptop");
        assert_eq!(value["secret"], secret);
    }

    /// No key material may reach a client, on the account or on any of its keys.
    ///
    /// The `AccountResource` is a copy of the store's own struct with the
    /// secrets left off, so this is the guard on that copy: adding a field to
    /// `Account` or `Key` would otherwise carry a hash straight into the JSON,
    /// and every read of the account list would hand one out.
    #[test]
    fn an_account_serialises_without_any_key_material() {
        let a = users::Account {
            provider: None,
            model: None,
            compact: crate::compact::Compact::None,
            tools: crate::tools::Policy::default(),
            id: "id-1".to_owned(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            email: "ada@example.org".to_owned(),
            created_at: 1,
            keys: vec![users::Key {
                id: "k-1".to_owned(),
                name: "laptop".to_owned(),
                hash: "deadbeef".to_owned(),
                created_at: 1,
                first_used_at: None,
                last_used_at: None,
                last_used_ip: None,
                disabled: false,
            }],
            key_hash: "cafebabe".to_owned(),
            legacy_first_used_at: None,
            legacy_last_used_at: None,
            legacy_last_used_ip: None,
            disabled: false,
            weight: 1,
        };
        let json = serde_json::to_string(&AccountResource::from(&a)).expect("serialize");

        assert!(!json.contains("deadbeef"), "a key hash reached the client: {json}");
        assert!(
            !json.contains("cafebabe"),
            "the account-level key hash reached the client: {json}"
        );
        assert!(json.contains("laptop"), "keys are listed by name: {json}");
        assert!(json.contains("ada@example.org"));
    }

    #[test]
    fn a_key_carries_the_identity_of_the_machine_that_used_it() {
        // This is what makes a leaked key findable: the operator sees which
        // address used it last without reading a log.
        let mut store = store_with("res-key-ip@example.com");
        let (mut key, _secret) = store.add_key("res-key-ip@example.com", "laptop").expect("mint");
        key.last_used_ip = Some("203.0.113.7".to_owned());
        key.last_used_at = Some(1_700_000_000);

        let r = KeyResource::from(&key);
        assert_eq!(r.last_used_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(r.last_used_at, Some(1_700_000_000));
    }
}
