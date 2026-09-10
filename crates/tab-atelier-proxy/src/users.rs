// SPDX-License-Identifier: MPL-2.0

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

/// One credential belonging to an account.
///
/// A person has several: a laptop, a CI runner, a fleet worker. That is not a
/// convenience — it is what makes revocation usable. With one key per person,
/// losing a laptop means re-keying everything that person runs; with a key per
/// place, it means deleting one row and leaving the rest working.
///
/// The dates and the address describe THE KEY, not the person, which is the
/// only level at which they mean anything: "last used from 203.0.113.7" says
/// nothing if three keys share an account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Key {
    pub id: String,
    /// What it is for — `laptop`, `ci`, `fleet`. The reason multiple keys are
    /// manageable at all: an unnamed list of hashes cannot be revoked with any
    /// confidence about what will break.
    pub name: String,
    /// Hex SHA-256. The key itself is shown once, at creation, and never
    /// stored.
    pub hash: String,
    pub created_at: u64,
    /// Issued and never used is the state worth seeing: either someone never
    /// took up their access, or the key went astray on the way to them.
    #[serde(default)]
    pub first_used_at: Option<u64>,
    #[serde(default)]
    pub last_used_at: Option<u64>,
    /// One address, not a history: enough to notice a key being used from
    /// somewhere it should not be, without turning the file into a movement
    /// log of the people using it.
    #[serde(default)]
    pub last_used_ip: Option<String>,
    /// Refused without being deleted, so the name stays attached to past use.
    #[serde(default)]
    pub disabled: bool,
}

impl Key {
    #[must_use]
    pub const fn active(&self) -> bool {
        !self.disabled && !self.hash.is_empty()
    }
}

/// A person who may use the proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub created_at: u64,
    /// Every credential this person holds. See [`Key`].
    #[serde(default)]
    pub keys: Vec<Key>,
    /// The single key this account used to have, read once and folded into
    /// `keys` on load. Never written again.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_hash: String,
    /// Provenance that used to live on the account, when there was one key.
    /// Read once during migration and then written on the key instead, where
    /// it means something.
    #[serde(default, rename = "first_used_at", skip_serializing_if = "Option::is_none")]
    pub legacy_first_used_at: Option<u64>,
    #[serde(default, rename = "last_used_at", skip_serializing_if = "Option::is_none")]
    pub legacy_last_used_at: Option<u64>,
    #[serde(default, rename = "last_used_ip", skip_serializing_if = "Option::is_none")]
    pub legacy_last_used_ip: Option<String>,
    /// Suspends the WHOLE account, whatever its keys say. Distinct from
    /// disabling one key: this is "this person is out", not "that laptop is
    /// gone".
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

    /// Usable right now: not suspended, and holding at least one live key.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.disabled && self.keys.iter().any(Key::active)
    }

    /// Fold a pre-multi-key account into one named key.
    ///
    /// Called on load so an upgrade keeps working: the old single hash becomes
    /// a key called `default`, carrying the provenance that used to sit on the
    /// account. Doing this at the boundary means nothing below has to know the
    /// old shape ever existed.
    fn migrate_single_key(&mut self) {
        if self.key_hash.is_empty() {
            return;
        }
        if !self.keys.iter().any(|k| k.hash == self.key_hash) {
            self.keys.push(Key {
                id: uuid::Uuid::new_v4().to_string(),
                name: "default".to_owned(),
                hash: std::mem::take(&mut self.key_hash),
                created_at: self.created_at,
                first_used_at: self.legacy_first_used_at,
                last_used_at: self.legacy_last_used_at,
                last_used_ip: self.legacy_last_used_ip.take(),
                disabled: false,
            });
        }
        self.key_hash = String::new();
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
    by_hash: BTreeMap<String, (usize, usize)>,
}

/// Why a mutation was refused. Callers turn these into an exit code or an HTTP
/// status, so the distinction has to survive out of here.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotFound(String),
    DuplicateEmail(String),
    DuplicateKeyName(String),
    InvalidEmail(String),
    MissingName,
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(who) => write!(f, "no such account: {who}"),
            Self::DuplicateEmail(e) => write!(f, "an account already uses {e}"),
            Self::DuplicateKeyName(n) => write!(f, "this account already has a key named {n}"),
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
        // Fold any pre-multi-key account into one named key before anything
        // else looks at it.
        for a in &mut store.accounts {
            a.migrate_single_key();
        }
        store.reindex();
        Ok(store)
    }

    fn reindex(&mut self) {
        self.by_hash = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.disabled)
            .flat_map(|(ai, a)| {
                a.keys
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| k.active())
                    .map(move |(ki, k)| (k.hash.clone(), (ai, ki)))
            })
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
        self.authenticate_key(key).map(|(a, _)| a)
    }

    /// The account AND which of its keys was used.
    ///
    /// Callers that record anything want the key, not just the person: usage
    /// is billed to the account, but "last used, and from where" belongs to
    /// the credential that was actually presented.
    #[must_use]
    pub fn authenticate_key(&self, key: &str) -> Option<(&Account, &Key)> {
        let hash = hash_key(key);
        let (ai, ki) = *self.by_hash.get(&hash)?;
        let account = self.accounts.get(ai)?;
        let k = account.keys.get(ki)?;
        (!account.disabled && k.active() && constant_time_eq(k.hash.as_bytes(), hash.as_bytes()))
            .then_some((account, k))
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

    /// Create an account, with no keys.
    ///
    /// It used to mint one called `default`. A key's name says WHERE it is
    /// used — that is the whole reason there is a key per place rather than
    /// per person, because revoking a lost laptop should be one row and not a
    /// re-keying. `default` says nothing, and being handed one at signup meant
    /// it was the one that got deployed, so the accounts that most needed
    /// named keys were the ones that never got them.
    ///
    /// `add-key <who> <place>` mints the first real one.
    ///
    /// # Errors
    /// Missing names, an unusable email, or an email already in use.
    pub fn add(&mut self, first: &str, last: &str, email: &str) -> Result<Account, Error> {
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
        let now = now_secs();
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            email: email.to_owned(),
            created_at: now,
            keys: Vec::new(),
            key_hash: String::new(),
            legacy_first_used_at: None,
            legacy_last_used_at: None,
            legacy_last_used_ip: None,
            disabled: false,
            weight: default_weight(),
        };
        self.accounts.push(account.clone());
        self.reindex();
        self.save()?;
        Ok(account)
    }

    /// Add a named key to an account. Returns it once, in readable form.
    ///
    /// This replaces the old `rotate`, and is strictly better: rotation
    /// revoked the only key and issued another, so there was a moment with no
    /// working credential and everything using it broke at once. Adding first
    /// and removing later means a machine can be moved across without a gap.
    ///
    /// # Errors
    /// No such account, a name already in use on this account, or the file
    /// could not be written.
    pub fn add_key(&mut self, who: &str, name: &str) -> Result<(Key, String), Error> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::MissingName);
        }
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
        // Names are how keys are revoked, so two of the same on one account
        // would make "delete the laptop key" ambiguous.
        if account.keys.iter().any(|k| k.name.eq_ignore_ascii_case(name)) {
            return Err(Error::DuplicateKeyName(name.to_owned()));
        }
        let secret = mint_key();
        let key = Key {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_owned(),
            hash: hash_key(&secret),
            created_at: now_secs(),
            first_used_at: None,
            last_used_at: None,
            last_used_ip: None,
            disabled: false,
        };
        account.keys.push(key.clone());
        self.reindex();
        self.save()?;
        Ok((key, secret))
    }

    /// Delete one key. The account and its other keys are untouched.
    ///
    /// # Errors
    /// No such account or key, or the file could not be written.
    pub fn remove_key(&mut self, who: &str, key_ref: &str) -> Result<Key, Error> {
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
        let idx = account
            .keys
            .iter()
            .position(|k| k.id == key_ref || k.name.eq_ignore_ascii_case(key_ref))
            .ok_or_else(|| Error::NotFound(format!("key {key_ref}")))?;
        let gone = account.keys.remove(idx);
        self.reindex();
        self.save()?;
        Ok(gone)
    }

    /// Turn one key off (or back on) without deleting it.
    ///
    /// # Errors
    /// No such account or key, or the file could not be written.
    pub fn set_key_disabled(&mut self, who: &str, key_ref: &str, disabled: bool) -> Result<Key, Error> {
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
        let key = account
            .keys
            .iter_mut()
            .find(|k| k.id == key_ref || k.name.eq_ignore_ascii_case(key_ref))
            .ok_or_else(|| Error::NotFound(format!("key {key_ref}")))?;
        key.disabled = disabled;
        let out = key.clone();
        self.reindex();
        self.save()?;
        Ok(out)
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

    /// Stamp the KEY that was used, from `ip`.
    ///
    /// Best-effort: a failed write must not fail the request it is describing.
    pub fn touch(&mut self, key_id: &str, ip: Option<&str>) {
        let now = now_secs();
        let persist = self
            .accounts
            .iter_mut()
            .flat_map(|a| a.keys.iter_mut())
            .find(|k| k.id == key_id)
            .is_some_and(|k| {
                // First use of a key, and any change of address, are worth a
                // write immediately — they are the two things someone
                // reviewing access actually looks for. Ordinary traffic is
                // coalesced to one write a minute, or every proxied request
                // would rewrite the file.
                let first = k.first_used_at.is_none();
                let moved = ip.is_some() && k.last_used_ip.as_deref() != ip;
                let stale = k.last_used_at.is_none_or(|t| now.saturating_sub(t) >= 60);
                if first {
                    k.first_used_at = Some(now);
                }
                if let Some(ip) = ip {
                    k.last_used_ip = Some(ip.to_owned());
                }
                k.last_used_at = Some(now);
                first || moved || stale
            });
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
        let ada = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        // Accounts start with no keys — one is minted per place it is used.
        let (_k, ada_key) = s.add_key(&ada.email, "laptop").expect("key");
        let grace = s.add("Grace", "Hopper", "grace@example.org").expect("add");
        // Accounts start with no keys — one is minted per place it is used.
        let (_k, grace_key) = s.add_key(&grace.email, "laptop").expect("key");

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
        // Accounts start with no keys — one is minted per place it is used.
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_, ada_key) = s.add_key("ada@example.org", "laptop").expect("key");
        s.add("Grace", "Hopper", "grace@example.org").expect("add");
        let (_, grace_key) = s.add_key("grace@example.org", "laptop").expect("key");

        s.set_disabled("ada@example.org", true).expect("disable");
        assert!(s.authenticate(&ada_key).is_none(), "a disabled account must be refused");
        assert!(s.authenticate(&grace_key).is_some(), "and nobody else is affected");

        s.set_disabled("ada@example.org", false).expect("enable");
        assert!(s.authenticate(&ada_key).is_some(), "re-enabling restores the same key");

        s.remove("grace@example.org").expect("remove");
        assert!(s.authenticate(&grace_key).is_none());
        assert!(s.authenticate(&ada_key).is_some());
    }

    /// Each key carries its own history, which is the only level at which it
    /// means anything: "last used from 203.0.113.7" says nothing when three
    /// keys share an account.
    #[test]
    fn each_key_records_when_and_where_it_was_used() {
        let (mut s, _d) = store();
        let ada = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        // Accounts start with no keys — one is minted per place it is used.
        let (_k, _first) = s.add_key(&ada.email, "laptop").expect("key");
        let (ci, _ci_secret) = s.add_key("ada@example.org", "ci").expect("add key");

        let laptop = s.find("ada@example.org").expect("a").keys[0].clone();
        assert_eq!(laptop.first_used_at, None, "issued and never used is worth seeing");

        s.touch(&laptop.id, Some("203.0.113.7"));
        let after = s.find("ada@example.org").expect("a").clone();
        let seen = after.keys.iter().find(|k| k.id == laptop.id).expect("laptop");
        assert!(seen.first_used_at.is_some());
        assert_eq!(seen.last_used_ip.as_deref(), Some("203.0.113.7"));

        // The OTHER key is untouched — that is the whole point of per-key
        // provenance.
        let other = after.keys.iter().find(|k| k.id == ci.id).expect("ci");
        assert_eq!(other.first_used_at, None);
        assert_eq!(other.last_used_ip, None);

        // A later call from elsewhere moves last_used_ip, never first_used.
        s.touch(&laptop.id, Some("198.51.100.4"));
        let moved = s.find("ada@example.org").expect("a").keys[0].clone();
        assert_eq!(moved.first_used_at, seen.first_used_at, "first use is set once");
        assert_eq!(moved.last_used_ip.as_deref(), Some("198.51.100.4"));
        assert_eq!(ada.email, "ada@example.org");
    }

    /// Several named keys, revoked one at a time. With a single key per
    /// person, losing a laptop meant re-keying everything they run.
    #[test]
    fn one_key_can_be_revoked_without_disturbing_the_others() {
        let (mut s, _d) = store();
        // An account starts with NO keys; every one is named for where it
        // lives. There used to be a "default" minted at signup, which is
        // exactly the key that ended up deployed everywhere unnamed.
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_k, first) = s.add_key("ada@example.org", "desktop").expect("desktop");
        let (_lk, laptop) = s.add_key("ada@example.org", "laptop").expect("laptop");
        let (_k, ci) = s.add_key("ada@example.org", "ci").expect("ci");
        let (_k2, fleet) = s.add_key("ada@example.org", "fleet").expect("fleet");
        for k in [&first, &laptop, &ci, &fleet] {
            assert!(s.authenticate(k).is_some(), "every key should work");
        }

        // Lose the laptop.
        s.remove_key("ada@example.org", "laptop").expect("remove");
        assert!(s.authenticate(&laptop).is_none(), "the lost key must stop working");
        assert!(s.authenticate(&ci).is_some(), "and nothing else is disturbed");
        assert!(s.authenticate(&fleet).is_some());
        assert!(s.authenticate(&first).is_some());

        // Disabling is the reversible form, and keeps the name attached.
        s.set_key_disabled("ada@example.org", "ci", true).expect("disable");
        assert!(s.authenticate(&ci).is_none());
        s.set_key_disabled("ada@example.org", "ci", false).expect("enable");
        assert!(s.authenticate(&ci).is_some(), "re-enabling restores the same key");

        // Suspending the PERSON stops all of them at once, which is a
        // different decision from revoking one credential.
        s.set_disabled("ada@example.org", true).expect("suspend");
        for k in [&ci, &fleet] {
            assert!(s.authenticate(k).is_none(), "a suspended account has no working keys");
        }
    }

    #[test]
    fn key_names_are_unique_within_an_account() {
        let (mut s, _d) = store();
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        s.add_key("ada@example.org", "ci").expect("ci");
        assert_eq!(
            s.add_key("ada@example.org", "CI").unwrap_err(),
            Error::DuplicateKeyName("CI".to_owned()),
            "names are how keys are revoked, so a duplicate makes revocation ambiguous"
        );
        // But two PEOPLE may each have a key called ci.
        s.add("Grace", "Hopper", "grace@example.org").expect("add");
        assert!(s.add_key("grace@example.org", "ci").is_ok());
    }

    /// An account written before keys were a list still works, and its
    /// provenance moves onto the key rather than being lost.
    #[test]
    fn a_single_key_account_migrates_to_one_named_key() {
        let dir = TempDir::new();
        let path = dir.path().join("users.json");
        let secret = mint_key();
        let doc = serde_json::json!({
            "accounts": [{
                "id": "old-1",
                "first_name": "Ada",
                "last_name": "Lovelace",
                "email": "ada@example.org",
                "key_hash": hash_key(&secret),
                "created_at": 1_700_000_000,
                "first_used_at": 1_700_000_100,
                "last_used_at": 1_700_000_200,
                "last_used_ip": "203.0.113.7",
                "disabled": false,
                "weight": 5
            }]
        });
        std::fs::write(&path, doc.to_string()).expect("write");

        let s = Store::load(&path).expect("load");
        let a = s.find("ada@example.org").expect("account survived");
        assert_eq!(a.keys.len(), 1, "the single key becomes one key");
        assert_eq!(a.keys[0].name, "default");
        assert_eq!(
            a.keys[0].last_used_ip.as_deref(),
            Some("203.0.113.7"),
            "the account's provenance moves onto the key, where it means something"
        );
        assert_eq!(a.keys[0].first_used_at, Some(1_700_000_100));
        assert!(
            s.authenticate(&secret).is_some(),
            "and the key that was working before must still work"
        );
    }

    #[test]
    fn keys_are_not_recoverable_from_the_file() {
        let (mut s, _d) = store();
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_, key) = s.add_key("ada@example.org", "laptop").expect("key");
        let raw = std::fs::read_to_string(s.path()).expect("read back");
        assert!(!raw.contains(&key), "the key itself must never reach disk");
        assert!(raw.contains(&hash_key(&key)));
    }

    #[test]
    fn accounts_survive_a_reload() {
        let (mut s, dir) = store();
        s.add("Ada", "Lovelace", "ada@example.org").expect("add");
        let (_, key) = s.add_key("ada@example.org", "laptop").expect("key");
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
        let a = s.add("Ada", "Lovelace", "ada@example.org").expect("add");
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
