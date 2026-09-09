// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! Leases over named keys — the primitive that lets a fleet of agents divide
//! work without two of them doing the same thing.
//!
//! The blackboard (`note`/`notes`) is monotonic: everything on it is an
//! observation, and observations merge without coordination. "I am the one
//! working on this" is the opposite — it is mutual exclusion, and it is the
//! one place in the system where coordination is genuinely required rather
//! than merely convenient.
//!
//! It is a **lease**, not a lock (Gray & Cheriton, SOSP '89): it expires. Agent
//! tabs die constantly — crashes, compaction, `cgroup.kill`, an operator
//! closing a tab — and a lock without expiry in a fleet of ephemeral holders
//! is a deadlock generator. A dead holder's claim simply lapses.
//!
//! Every grant carries a monotonically increasing **fence** token. A holder
//! that stalls past its expiry can come back and try to finish work it no
//! longer owns; whoever cares can compare fences and reject the stale one.
//!
//! There is no consensus here and there does not need to be: one daemon owns
//! the keyspace on its host, which is the same shape as a Chubby cell's master
//! (Burrows, OSDI '06) — a lock service, not a library.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default lease length when the caller doesn't say.
pub const DEFAULT_TTL_MS: u64 = 300_000;
/// Shortest lease worth granting — below this the holder spends its life
/// renewing.
pub const MIN_TTL_MS: u64 = 1_000;
/// Longest lease. A day-long claim from a crashed agent is indistinguishable
/// from a deadlock, so the ceiling is deliberately low enough to self-heal
/// within a working session.
pub const MAX_TTL_MS: u64 = 6 * 3_600_000;
/// Cap on key length — keys are task ids and file paths, not documents.
pub const MAX_KEY: usize = 200;
/// Cap on holder length (a tab name or uuid).
pub const MAX_HOLDER: usize = 100;

/// One granted lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub key: String,
    /// Whoever asked — by convention a tab name or uuid, but the registry
    /// treats it as an opaque identity string.
    pub holder: String,
    pub granted_ms: u64,
    pub expires_ms: u64,
    /// Strictly increasing across every grant this registry makes. A holder
    /// presenting an old fence is presenting a claim it has already lost.
    pub fence: u64,
}

impl Claim {
    /// Whether the lease is still live at `now_ms`.
    #[must_use]
    pub const fn live_at(&self, now_ms: u64) -> bool {
        self.expires_ms > now_ms
    }

    /// Milliseconds left, saturating at zero.
    #[must_use]
    pub const fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.expires_ms.saturating_sub(now_ms)
    }
}

/// Why a grant was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// Someone else holds a live lease. Carries the current holder so the
    /// caller can report who to wait for rather than just "no".
    Held(Claim),
    /// The key or holder failed validation.
    Invalid(String),
}

/// The host's lease table.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Registry {
    claims: BTreeMap<String, Claim>,
    /// Never reset, including across a reload — fences must not repeat, or a
    /// stale holder's token could compare equal to a live one.
    next_fence: u64,
}

/// Reject keys that would make the registry hard to reason about: empty,
/// oversized, or carrying control characters that would mangle a log line.
fn sanitize(what: &str, s: &str, max: usize) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if t.chars().count() > max {
        return Err(format!("{what} is longer than {max} characters"));
    }
    if t.chars().any(char::is_control) {
        return Err(format!("{what} contains control characters"));
    }
    Ok(t.to_owned())
}

impl Registry {
    /// Take the lease on `key` for `holder`.
    ///
    /// Granted when the key is free, its lease has lapsed, or `holder` already
    /// holds it (a renewal — idempotent, so an agent can refresh on a timer
    /// without tracking whether it still owns the key).
    ///
    /// # Errors
    /// [`GrantError::Held`] when someone else's lease is live, or
    /// [`GrantError::Invalid`] when key/holder fail validation.
    pub fn grant(&mut self, key: &str, holder: &str, ttl_ms: u64, now_ms: u64) -> Result<Claim, GrantError> {
        let key = sanitize("key", key, MAX_KEY).map_err(GrantError::Invalid)?;
        let holder = sanitize("holder", holder, MAX_HOLDER).map_err(GrantError::Invalid)?;
        let ttl = ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS);
        if let Some(cur) = self.claims.get(&key)
            && cur.live_at(now_ms)
            && cur.holder != holder
        {
            return Err(GrantError::Held(cur.clone()));
        }
        self.next_fence += 1;
        let claim = Claim {
            key: key.clone(),
            holder,
            granted_ms: now_ms,
            expires_ms: now_ms.saturating_add(ttl),
            fence: self.next_fence,
        };
        self.claims.insert(key, claim.clone());
        Ok(claim)
    }

    /// Give up a lease early. Only the holder may release: a lapsed holder
    /// must not be able to release the key out from under whoever took it
    /// next, which is the classic lock-service bug fencing exists to catch.
    pub fn release(&mut self, key: &str, holder: &str, now_ms: u64) -> bool {
        let Some(cur) = self.claims.get(key) else { return false };
        if cur.holder != holder || !cur.live_at(now_ms) {
            return false;
        }
        self.claims.remove(key).is_some()
    }

    /// The live lease on `key`, if any.
    #[must_use]
    pub fn get(&self, key: &str, now_ms: u64) -> Option<&Claim> {
        self.claims.get(key).filter(|c| c.live_at(now_ms))
    }

    /// Every live lease, key order.
    #[must_use]
    pub fn active(&self, now_ms: u64) -> Vec<Claim> {
        self.claims.values().filter(|c| c.live_at(now_ms)).cloned().collect()
    }

    /// Drop lapsed entries. Purely housekeeping — [`Registry::get`] and
    /// [`Registry::active`] already ignore them, so correctness never depends
    /// on this having run.
    pub fn gc(&mut self, now_ms: u64) {
        self.claims.retain(|_, c| c.live_at(now_ms));
    }

    /// Live lease count.
    #[must_use]
    pub fn len(&self, now_ms: u64) -> usize {
        self.claims.values().filter(|c| c.live_at(now_ms)).count()
    }

    #[must_use]
    pub fn is_empty(&self, now_ms: u64) -> bool {
        self.len(now_ms) == 0
    }
}

static REGISTRY: std::sync::RwLock<Option<Registry>> = std::sync::RwLock::new(None);

/// Test/ops override for where the lease table is persisted. Set via
/// [`set_registry_path`].
static PATH_OVERRIDE: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// Point the registry at a different file. Tests use a tempdir so a run never
/// touches the developer's real state directory; `None` restores the default.
pub fn set_registry_path(path: Option<std::path::PathBuf>) {
    if let Ok(mut g) = PATH_OVERRIDE.write() {
        *g = path;
    }
}

fn registry_path() -> std::path::PathBuf {
    if let Some(p) = PATH_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return p;
    }
    crate::platform::state_base_dir()
        .join(crate::APP_DIR)
        .join("claims.json")
}

/// Run `f` against the host registry, loading it from disk on first use and
/// writing it back if `f` changed it.
///
/// Persisting matters for one case: a daemon restart must not hand the same
/// key to two agents, since tabs outlive the daemon. Lapsed leases are dropped
/// on load, so the file can never resurrect a dead claim.
pub fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    // Serialise inside the lock, write outside it: a grant must not wait on
    // the filesystem, and a slow disk must not serialise the whole fleet's
    // claims behind one writer.
    // Serialise inside the lock, write outside it: a grant must not wait on
    // the filesystem, and a slow disk must not serialise the whole fleet's
    // claims behind one writer. The read-modify-write itself has to stay one
    // critical section, or two agents racing for a key could both see it free.
    let mut guard = REGISTRY.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        let loaded = std::fs::read_to_string(registry_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Registry>(&s).ok())
            .unwrap_or_default();
        *guard = Some(loaded);
    }
    let reg = guard.get_or_insert_with(Registry::default);
    reg.gc(crate::unix_millis());
    let before = reg.clone();
    let out = f(reg);
    let changed = reg.claims != before.claims || reg.next_fence != before.next_fence;
    let to_persist = changed.then(|| serde_json::to_string(reg).ok()).flatten();
    drop(guard);
    if let Some(body) = to_persist {
        let path = registry_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Best-effort, like the token write: a read-only state dir must not
        // stop leases from working in memory for this daemon's lifetime.
        let _ = std::fs::write(&path, body);
    }
    out
}

/// Drop the in-memory registry so the next [`with_registry`] reloads. Tests
/// only — the registry is process-global.
#[cfg(test)]
pub fn reset_for_test() {
    *REGISTRY.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Registry::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    #[test]
    fn one_holder_at_a_time_and_the_loser_learns_who_won() {
        let mut r = Registry::default();
        let a = r.grant("cov:src/api.rs", "tab-a", 10_000, T0).unwrap();
        assert_eq!(a.holder, "tab-a");
        // The whole point: the second agent is refused…
        let err = r.grant("cov:src/api.rs", "tab-b", 10_000, T0).unwrap_err();
        // …and told who to wait for, so it can pick different work instead of
        // retrying blind.
        match err {
            GrantError::Held(cur) => assert_eq!(cur.holder, "tab-a"),
            GrantError::Invalid(e) => panic!("expected Held, got {e}"),
        }
        // A different key is unaffected — this is per-key exclusion.
        assert!(r.grant("cov:src/app.rs", "tab-b", 10_000, T0).is_ok());
    }

    #[test]
    fn a_lease_lapses_so_a_dead_agent_cannot_block_the_key_forever() {
        let mut r = Registry::default();
        r.grant("k", "tab-a", 5_000, T0).unwrap();
        // Still held one millisecond before expiry…
        assert!(r.grant("k", "tab-b", 5_000, T0 + 4_999).is_err());
        assert!(r.get("k", T0 + 4_999).is_some());
        // …and free at it. `tab-a` may well still be running; that is exactly
        // the trade a lease makes, and why the fence exists.
        let b = r.grant("k", "tab-b", 5_000, T0 + 5_000).unwrap();
        assert_eq!(b.holder, "tab-b");
        assert!(r.get("k", T0 + 5_000).is_some_and(|c| c.holder == "tab-b"));
        assert_eq!(r.len(T0 + 5_000), 1, "the lapsed claim was replaced, not doubled");
    }

    #[test]
    fn renewing_is_idempotent_for_the_holder() {
        let mut r = Registry::default();
        let first = r.grant("k", "tab-a", 5_000, T0).unwrap();
        // An agent refreshing on a timer must not have to know whether it
        // still owns the key.
        let again = r.grant("k", "tab-a", 5_000, T0 + 1_000).unwrap();
        assert_eq!(again.holder, "tab-a");
        assert_eq!(again.expires_ms, T0 + 6_000, "renewal extends from now");
        assert!(again.fence > first.fence, "every grant advances the fence");
    }

    #[test]
    fn fences_never_repeat_or_go_backwards() {
        let mut r = Registry::default();
        let mut seen = Vec::new();
        for i in 0..5 {
            let c = r.grant(&format!("k{i}"), "h", 1_000, T0).unwrap();
            seen.push(c.fence);
        }
        // Strictly increasing: a stale holder's token can always be told apart
        // from a live one by comparison alone.
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "{seen:?}");
        // Even after the key is reused by a new holder.
        let old = r.grant("k0", "h2", 1_000, T0 + 2_000).unwrap();
        assert!(old.fence > seen[4]);
    }

    #[test]
    fn only_the_holder_can_release() {
        let mut r = Registry::default();
        r.grant("k", "tab-a", 5_000, T0).unwrap();
        // A lapsed or unrelated agent releasing someone else's key is the
        // classic lock-service bug; refuse it.
        assert!(!r.release("k", "tab-b", T0), "a non-holder must not release");
        assert!(r.get("k", T0).is_some());
        assert!(!r.release("nosuch", "tab-a", T0), "unknown key");
        assert!(r.release("k", "tab-a", T0), "the holder may");
        assert!(r.get("k", T0).is_none());
        assert!(
            !r.release("k", "tab-a", T0),
            "releasing twice is not an error, just false"
        );
    }

    #[test]
    fn a_holder_whose_lease_lapsed_cannot_release_the_next_holders_key() {
        let mut r = Registry::default();
        r.grant("k", "tab-a", 1_000, T0).unwrap();
        let now = T0 + 2_000;
        r.grant("k", "tab-b", 5_000, now).unwrap();
        // tab-a wakes up late and tidies up after itself. It must not take
        // tab-b's lease with it.
        assert!(!r.release("k", "tab-a", now));
        assert!(r.get("k", now).is_some_and(|c| c.holder == "tab-b"));
    }

    #[test]
    fn keys_and_holders_are_validated_and_ttls_clamped() {
        let mut r = Registry::default();
        assert!(matches!(r.grant("", "h", 1_000, T0), Err(GrantError::Invalid(_))));
        assert!(matches!(r.grant("k", "  ", 1_000, T0), Err(GrantError::Invalid(_))));
        assert!(matches!(
            r.grant(&"x".repeat(MAX_KEY + 1), "h", 1_000, T0),
            Err(GrantError::Invalid(_))
        ));
        assert!(matches!(r.grant("a\nb", "h", 1_000, T0), Err(GrantError::Invalid(_))));
        // Surrounding whitespace is normalised, so " k" and "k" are one key
        // rather than two agents each believing they own it.
        let c = r.grant("  k  ", "  h  ", 1_000, T0).unwrap();
        assert_eq!(c.key, "k");
        assert_eq!(c.holder, "h");
        // A zero TTL would be a lease that never existed; a year-long one a
        // deadlock. Both clamp into the useful range.
        let short = r.grant("s", "h", 0, T0).unwrap();
        assert_eq!(short.remaining_ms(T0), MIN_TTL_MS);
        let long = r.grant("l", "h", u64::MAX, T0).unwrap();
        assert_eq!(long.remaining_ms(T0), MAX_TTL_MS);
    }

    #[test]
    fn listing_and_gc_ignore_lapsed_leases() {
        let mut r = Registry::default();
        r.grant("live", "h", 10_000, T0).unwrap();
        r.grant("dead", "h", 1_000, T0).unwrap();
        let now = T0 + 5_000;
        let active = r.active(now);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, "live");
        assert!(!r.is_empty(now));
        // gc is housekeeping: the answers above were already right without it.
        r.gc(now);
        assert_eq!(r.active(now).len(), 1);
        assert!(Registry::default().is_empty(now));
    }

    #[test]
    fn the_registry_survives_a_reload_without_resurrecting_dead_claims() {
        let mut r = Registry::default();
        r.grant("live", "h", 3_600_000, T0).unwrap();
        r.grant("dead", "h", 1_000, T0).unwrap();
        let encoded = serde_json::to_string(&r).unwrap();
        let mut back: Registry = serde_json::from_str(&encoded).unwrap();
        let now = T0 + 60_000;
        // A restart must not hand a live key to a second agent…
        assert!(matches!(
            back.grant("live", "other", 1_000, now),
            Err(GrantError::Held(_))
        ));
        // …nor keep a lapsed one out of circulation.
        assert!(back.grant("dead", "other", 1_000, now).is_ok());
        // Fences continue from where they left off rather than restarting at
        // 1, which would make a pre-restart token compare equal to a new one.
        let c = back.grant("third", "h", 1_000, now).unwrap();
        assert!(c.fence > 2, "fence continued across the reload: {}", c.fence);
    }
}
