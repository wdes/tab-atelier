// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Accounts and their keys — what replaced the single relay token.
//!
//! One shared secret for a whole fleet answers no useful question. It cannot
//! say who spent the quota, it cannot be taken away from one laptop without
//! re-keying every other, and a machine that leaves keeps working. An account
//! per person, with a key per account, answers all three.
//!
//! # Why keys are stored hashed
//!
//! A key here is 32 bytes straight from the CSPRNG, so there is no dictionary
//! to run against a hash of it: the only attack is exhaustive search of a
//! 256-bit space. That is why plain SHA-256 is the right choice and a slow KDF
//! (argon2, bcrypt) is not — those exist to make *low-entropy human passwords*
//! expensive to guess, and buy nothing against a random token while costing
//! real latency on every single proxied request.
//!
//! The consequence is deliberate: a key is displayed once, when it is minted,
//! and never again. The store cannot show it to you later because it does not
//! have it. Lost key → rotate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Prefix on every minted key, so one is recognisable in a log or an env var
/// and greppable when it leaks.
pub const KEY_PREFIX: &str = "tap_";

/// A person who may use the proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    /// Hex SHA-256 of the key. Empty until one is minted.
    #[serde(default)]
    pub key_hash: String,
    pub created_at: u64,
    /// When the CURRENT key was first used, and last used. Both reset on
    /// rotation, because they describe the key rather than the person: after a
    /// rotate, "first used" answering with the old key's date would be a lie
    /// about the credential you are looking at.
    ///
    /// A key created and never used is the interesting case — it means someone
    /// was issued access they never took up, or the key went astray on the way
    /// to them.
    #[serde(default)]
    pub first_used_at: Option<u64>,
    /// Set on every accepted request, so an operator can see which accounts are
    /// dormant before revoking them. Coarse (seconds) on purpose — this is not
    /// an audit log, and a precise one would make the file a write hotspot.
    #[serde(default)]
    pub last_used_at: Option<u64>,
    /// Where the current key was last used from.
    ///
    /// One address, not a history: enough to notice a key being used from
    /// somewhere it should not be, without turning the account file into a
    /// movement log of the people using it.
    #[serde(default)]
    pub last_used_ip: Option<String>,
    /// Refused without being deleted: keeps the name attached to past usage
    /// while stopping the key today.
    #[serde(default)]
    pub disabled: bool,
    /// Share of the upstream quota under contention, relative to everyone
    /// else's. 1 unless someone decided otherwise; see [`crate::qos`].
    ///
    /// It only matters when capacity binds — with spare quota a weight-1
    /// account still gets everything it asks for, because the scheduler is
    /// work-conserving.
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// Normal, on the scale the UI presents.
///
/// 5 rather than 1 so there is room BELOW the default: a CI account or a
/// backlog-grinding fleet should be able to yield to people without everyone
/// else having to be promoted. A scale whose default is also its floor can
/// only express "raise someone", which is the same decision seen from the
/// wrong end.
///
/// The ladder the UI offers is 1 / 2 / 5 / 10 / 20 / 50 — roughly geometric,
/// so each step is a real difference rather than a rounding one. Any value in
/// 1..=100 is accepted; the API is not restricted to the ladder.
pub const NORMAL_WEIGHT: u32 = 5;

const fn default_weight() -> u32 {
    NORMAL_WEIGHT
}

impl Account {
    #[must_use]
    pub fn display_name(&self) -> String {
        let full = format!("{} {}", self.first_name.trim(), self.last_name.trim());
        let full = full.trim().to_owned();
        if full.is_empty() { self.email.clone() } else { full }
    }

    /// Usable right now: enabled, and actually has a key.
    #[must_use]
    pub const fn active(&self) -> bool {
        !self.disabled && !self.key_hash.is_empty()
    }
}

/// What the account file holds. A struct rather than a bare list so a future
/// field (quotas, groups) does not need a migration.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    accounts: Vec<Account>,
}

/// The account store, backed by a JSON file.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    accounts: Vec<Account>,
    /// hash → index, rebuilt on load and after every mutation. Authentication
    /// happens on every proxied request; a linear scan over the accounts would
    /// also make the comparison's cost depend on position in the file.
    by_hash: BTreeMap<String, usize>,
}

/// Why a mutation was refused. Callers turn these into an exit code or an HTTP
/// status, so the distinction has to survive out of here.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotFound(String),
    DuplicateEmail(String),
    InvalidEmail(String),
    MissingName,
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(who) => write!(f, "no such account: {who}"),
            Self::DuplicateEmail(e) => write!(f, "an account already uses {e}"),
            Self::InvalidEmail(e) => write!(f, "not an email address: {e}"),
            Self::MissingName => write!(f, "first and last name are both required"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Hex SHA-256 — how a key is compared against the file.
#[must_use]
pub fn hash_key(key: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// 32 CSPRNG bytes, hex, behind [`KEY_PREFIX`].
///
/// Reads `/dev/urandom` directly and exits rather than falling back to
/// anything weaker: a guessable key on a credential proxy is worse than a
/// process that refuses to start.
#[must_use]
pub fn mint_key() -> String {
    use std::fmt::Write as _;
    use std::io::Read as _;
    let mut buf = [0u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("fatal: cannot read /dev/urandom to mint a key: {e}");
            std::process::exit(1);
        }
    }
    let mut out = String::with_capacity(KEY_PREFIX.len() + 64);
    out.push_str(KEY_PREFIX);
    for b in &buf {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Constant-time compare, so a wrong key cannot be improved a byte at a time
/// by timing the answer.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The shape an email has to have to be stored. Deliberately not RFC 5322:
/// this is a typo guard for an operator typing on a CLI, not an authority on
/// what an address may contain, and a strict parser here would reject valid
/// addresses for no gain.
fn email_ok(email: &str) -> bool {
    let email = email.trim();
    if email.len() < 3 || email.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

impl Store {
    /// Load the store, or start an empty one if the file is not there yet.
    ///
    /// # Errors
    /// Unreadable or malformed file. A malformed file is NOT silently replaced
    /// with an empty one — that would revoke every account on a typo.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let accounts = match std::fs::read_to_string(&path) {
            Ok(raw) if raw.trim().is_empty() => Vec::new(),
            Ok(raw) => {
                serde_json::from_str::<Doc>(&raw)
                    .map_err(|e| Error::Io(format!("{} is not valid account JSON: {e}", path.display())))?
                    .accounts
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Io(format!("read {}: {e}", path.display()))),
        };
        let mut store = Self {
            path,
            accounts,
            by_hash: BTreeMap::new(),
        };
        store.reindex();
        Ok(store)
    }

    fn reindex(&mut self) {
        self.by_hash = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.active())
            .map(|(i, a)| (a.key_hash.clone(), i))
            .collect();
    }

    #[must_use]
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    /// The account a key belongs to, if it is live.
    ///
    /// The map lookup finds the candidate; the constant-time compare is what
    /// decides. Looking up by hash first means an attacker learns nothing from
    /// how long a rejection took, because a wrong key simply is not in the map.
    #[must_use]
    pub fn authenticate(&self, key: &str) -> Option<&Account> {
        let hash = hash_key(key);
        let idx = *self.by_hash.get(&hash)?;
        let account = self.accounts.get(idx)?;
        (account.active() && constant_time_eq(account.key_hash.as_bytes(), hash.as_bytes())).then_some(account)
    }

    /// Find by id, or by email, or by unique case-insensitive name fragment.
    #[must_use]
    pub fn find(&self, who: &str) -> Option<&Account> {
        let needle = who.trim().to_lowercase();
        self.accounts
            .iter()
            .find(|a| a.id == who || a.email.to_lowercase() == needle)
            .or_else(|| {
                let mut hits = self
                    .accounts
                    .iter()
                    .filter(|a| a.display_name().to_lowercase().contains(&needle));
                let first = hits.next()?;
                // Ambiguous is not a match: acting on the wrong person's key
                // is worse than making the operator be specific.
                hits.next().is_none().then_some(first)
            })
    }

    /// Create an account and mint its first key. Returns the key — the only
    /// time it exists in readable form.
    ///
    /// # Errors
    /// Missing names, an unusable email, or an email already in use.
    pub fn add(&mut self, first: &str, last: &str, email: &str) -> Result<(Account, String), Error> {
        let (first, last, email) = (first.trim(), last.trim(), email.trim());
        if first.is_empty() || last.is_empty() {
            return Err(Error::MissingName);
        }
        if !email_ok(email) {
            return Err(Error::InvalidEmail(email.to_owned()));
        }
        if self.accounts.iter().any(|a| a.email.eq_ignore_ascii_case(email)) {
            return Err(Error::DuplicateEmail(email.to_owned()));
        }
        let key = mint_key();
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            email: email.to_owned(),
            key_hash: hash_key(&key),
            created_at: now_secs(),
            first_used_at: None,
            last_used_at: None,
            last_used_ip: None,
            disabled: false,
            weight: default_weight(),
        };
        self.accounts.push(account.clone());
        self.reindex();
        self.save()?;
        Ok((account, key))
    }

    /// Replace an account's key. The old one stops working immediately.
    ///
    /// # Errors
    /// No such account, or the file could not be written.
    pub fn rotate(&mut self, who: &str) -> Result<(Account, String), Error> {
        let id = self
            .find(who)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?
            .id
            .clone();
        let key = mint_key();
        let hash = hash_key(&key);
        let account = self
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?;
        account.key_hash = hash;
        // A new key has its own history. Carrying the old key's dates over
        // would misreport when THIS credential was first seen — which is the
        // one question these fields exist to answer.
        account.first_used_at = None;
        account.last_used_at = None;
        account.last_used_ip = None;
        let out = account.clone();
        self.reindex();
        self.save()?;
        Ok((out, key))
    }

    /// Change an account's share of the quota under contention.
    ///
    /// # Errors
    /// No such account, or the file could not be written.
    pub fn set_weight(&mut self, who: &str, weight: u32) -> Result<Account, Error> {
        let id = self
            .find(who)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?
            .id
            .clone();
        let account = self
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?;
        // Zero would mean "never scheduled", which is what `disable` is for
        // and is far too easy to do by accident.
        account.weight = weight.clamp(1, 100);
        let out = account.clone();
        self.save()?;
        Ok(out)
    }

    /// Turn an account off (or back on) without losing who they were.
    ///
    /// # Errors
    /// No such account, or the file could not be written.
    pub fn set_disabled(&mut self, who: &str, disabled: bool) -> Result<Account, Error> {
        let id = self
            .find(who)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?
            .id
            .clone();
        let account = self
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?;
        account.disabled = disabled;
        let out = account.clone();
        self.reindex();
        self.save()?;
        Ok(out)
    }

    /// Forget an account entirely.
    ///
    /// # Errors
    /// No such account, or the file could not be written.
    pub fn remove(&mut self, who: &str) -> Result<Account, Error> {
        let id = self
            .find(who)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?
            .id
            .clone();
        let idx = self
            .accounts
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| Error::NotFound(who.to_owned()))?;
        let gone = self.accounts.remove(idx);
        self.reindex();
        self.save()?;
        Ok(gone)
    }

    /// Stamp an account as having been used, from `ip`.
    ///
    /// Best-effort: a failed write must not fail the request it is describing.
    pub fn touch(&mut self, id: &str, ip: Option<&str>) {
        let persist = if let Some(a) = self.accounts.iter_mut().find(|a| a.id == id) {
            let now = now_secs();
            // First use of a key, and any change of address, are worth a write
            // immediately — they are the two things someone reviewing access
            // actually looks for. Ordinary traffic is coalesced to one write a
            // minute, or every proxied request would rewrite the file.
            let first = a.first_used_at.is_none();
            let moved = ip.is_some() && a.last_used_ip.as_deref() != ip;
            let stale = a.last_used_at.is_none_or(|t| now.saturating_sub(t) >= 60);
            if first {
                a.first_used_at = Some(now);
            }
            if let Some(ip) = ip {
                a.last_used_ip = Some(ip.to_owned());
            }
            a.last_used_at = Some(now);
            first || moved || stale
        } else {
            false
        };
        if persist {
            let _ = self.save();
        }
    }

    /// Write the file: temp + rename, 0600.
    ///
    /// # Errors
    /// Anything that stops the file reaching disk intact.
    pub fn save(&self) -> Result<(), Error> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("create {}: {e}", dir.display())))?;
        }
        let json = serde_json::to_string_pretty(&Doc {
            accounts: self.accounts.clone(),
        })
        .map_err(|e| Error::Io(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| Error::Io(format!("write {}: {e}", tmp.display())))?;
        // Permissions BEFORE the rename: between rename and chmod the real file
        // would briefly be world-readable, and it holds every key hash.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| Error::Io(format!("rename into {}: {e}", self.path.display())))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal scratch dir — the crate carries no dev-dependencies and a
    /// unique path plus cleanup on drop is all a test needs from one.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("ta-proxy-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).expect("mkdir");
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store() -> (Store, TempDir) {
        let dir = TempDir::new();
        let s = Store::load(dir.path().join("users.json")).expect("load");
        (s, dir)
    }

    #[test]
    fn a_key_authenticates_exactly_its_own_account() {
        let (mut s, _d) = store();
        let (ada, ada_key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_grace, grace_key) = s.add("Grace", "Hopper", "grace@example.org").expect("add");

        assert_eq!(s.authenticate(&ada_key).map(|a| a.id.clone()), Some(ada.id.clone()));
        assert_ne!(s.authenticate(&grace_key).map(|a| a.id.clone()), Some(ada.id));
        assert!(s.authenticate("tap_deadbeef").is_none());
        assert!(s.authenticate("").is_none());
    }

    /// The point of per-user keys: one person's key can be taken away without
    /// touching anybody else's. The mono-token setup could not do this.
    #[test]
    fn revoking_one_account_leaves_the_others_working() {
        let (mut s, _d) = store();
        let (_, ada_key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_, grace_key) = s.add("Grace", "Hopper", "grace@example.org").expect("add");

        s.set_disabled("ada@example.org", true).expect("disable");
        assert!(s.authenticate(&ada_key).is_none(), "a disabled account must be refused");
        assert!(s.authenticate(&grace_key).is_some(), "and nobody else is affected");

        s.set_disabled("ada@example.org", false).expect("enable");
        assert!(s.authenticate(&ada_key).is_some(), "re-enabling restores the same key");

        s.remove("grace@example.org").expect("remove");
        assert!(s.authenticate(&grace_key).is_none());
        assert!(s.authenticate(&ada_key).is_some());
    }

    /// A key's own history: when it was first used, last used, and from where.
    #[test]
    fn a_key_records_when_and_where_it_was_used() {
        let (mut s, _d) = store();
        let (ada, key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        // Issued but never used is a state worth being able to see.
        assert_eq!(ada.first_used_at, None);
        assert_eq!(ada.last_used_ip, None);

        s.touch(&ada.id, Some("203.0.113.7"));
        let seen = s.find("ada@example.org").expect("account").clone();
        assert!(seen.first_used_at.is_some());
        assert_eq!(
            seen.last_used_at, seen.first_used_at,
            "first use is also the last so far"
        );
        assert_eq!(seen.last_used_ip.as_deref(), Some("203.0.113.7"));

        // A later call from elsewhere moves last_used_ip but never first_used.
        s.touch(&ada.id, Some("198.51.100.4"));
        let moved = s.find("ada@example.org").expect("account").clone();
        assert_eq!(moved.first_used_at, seen.first_used_at, "first use is set once");
        assert_eq!(moved.last_used_ip.as_deref(), Some("198.51.100.4"));
        assert!(s.authenticate(&key).is_some(), "none of this disturbs the key");
    }

    /// The dates describe the KEY, not the person: after a rotate, reporting
    /// when the old key was first used would be a lie about the credential
    /// being looked at.
    #[test]
    fn rotating_starts_the_history_over() {
        let (mut s, _d) = store();
        let (ada, _key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        s.touch(&ada.id, Some("203.0.113.7"));
        assert!(s.find("ada@example.org").expect("a").first_used_at.is_some());

        let (rotated, _new_key) = s.rotate("ada@example.org").expect("rotate");
        assert_eq!(rotated.first_used_at, None);
        assert_eq!(rotated.last_used_at, None);
        assert_eq!(rotated.last_used_ip, None);
        // The account itself is untouched — this is the key's history, not the
        // person's.
        assert_eq!(rotated.email, "ada@example.org");
        assert_eq!(rotated.created_at, ada.created_at);
    }

    #[test]
    fn rotating_invalidates_the_previous_key() {
        let (mut s, _d) = store();
        let (_, old) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_, new) = s.rotate("ada@example.org").expect("rotate");
        assert_ne!(old, new);
        assert!(s.authenticate(&old).is_none(), "the old key must stop working at once");
        assert!(s.authenticate(&new).is_some());
    }

    #[test]
    fn keys_are_not_recoverable_from_the_file() {
        let (mut s, _d) = store();
        let (_, key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let raw = std::fs::read_to_string(s.path()).expect("read back");
        assert!(!raw.contains(&key), "the key itself must never reach disk");
        assert!(raw.contains(&hash_key(&key)));
    }

    #[test]
    fn accounts_survive_a_reload() {
        let (mut s, dir) = store();
        let (_, key) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        drop(s);
        let s = Store::load(dir.path().join("users.json")).expect("reload");
        assert_eq!(s.accounts().len(), 1);
        assert!(s.authenticate(&key).is_some(), "the hash index must be rebuilt on load");
    }

    /// A corrupt file must not read as "no accounts": that silently revokes
    /// everyone and looks like a working proxy that rejects every request.
    #[test]
    fn a_malformed_file_is_an_error_not_an_empty_store() {
        let dir = TempDir::new();
        let path = dir.path().join("users.json");
        std::fs::write(&path, "{ this is not json").expect("write");
        assert!(Store::load(&path).is_err());
    }

    #[test]
    fn duplicate_and_malformed_details_are_refused() {
        let (mut s, _d) = store();
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        assert_eq!(
            s.add("Ada", "Byron", "ADA@example.org").unwrap_err(),
            Error::DuplicateEmail("ADA@example.org".to_owned()),
            "email match is case-insensitive, or one person gets two accounts"
        );
        assert_eq!(s.add("", "Lovelace", "x@example.org").unwrap_err(), Error::MissingName);
        assert!(matches!(
            s.add("Ada", "Lovelace", "not-an-email").unwrap_err(),
            Error::InvalidEmail(_)
        ));
        assert!(matches!(
            s.add("Ada", "Lovelace", "a@b").unwrap_err(),
            Error::InvalidEmail(_)
        ));
    }

    #[test]
    fn an_ambiguous_name_matches_nobody() {
        let (mut s, _d) = store();
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        s.add("Ada", "Byron", "byron@example.org").expect("add");
        assert!(s.find("ada").is_none(), "two people match — acting on either is wrong");
        assert!(s.find("lovelace").is_some());
        assert!(s.find("ada@example.org").is_some());
    }

    /// The priority ladder must have room below the default, or "lower this
    /// account" is inexpressible and the only move is promoting everyone else.
    #[test]
    fn the_default_priority_sits_in_the_middle_of_its_range() {
        let (mut s, _d) = store();
        let (a, _k) = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        assert_eq!(a.weight, NORMAL_WEIGHT);
        const { assert!(NORMAL_WEIGHT > 1, "a default of 1 leaves nothing below it") }

        // Both directions are reachable, and the ratio is a real difference.
        let down = s.set_weight("ada@example.org", 1).expect("lower");
        assert_eq!(down.weight, 1);
        let up = s.set_weight("ada@example.org", 50).expect("raise");
        assert_eq!(up.weight, 50);

        // Out-of-range values are clamped rather than rejected — and never to
        // zero, which would mean "never scheduled" and is what `disable` is
        // for.
        assert_eq!(s.set_weight("ada@example.org", 0).expect("clamp").weight, 1);
        assert_eq!(s.set_weight("ada@example.org", 9_999).expect("clamp").weight, 100);
    }

    #[test]
    fn minted_keys_are_distinct_and_prefixed() {
        let a = mint_key();
        let b = mint_key();
        assert_ne!(a, b);
        assert!(a.starts_with(KEY_PREFIX));
        assert_eq!(a.len(), KEY_PREFIX.len() + 64);
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
