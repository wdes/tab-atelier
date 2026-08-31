// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `catalog` — the retired-agent CARD catalogue + persist-gated de-register (RB1).
//!
//! When an agent is RETIRED, the daemon COPIES its card (the durable
//! build_snapshot fields) into `catalog.jsonl`, RE-READS it (read-back, not "I
//! wrote it"), and only THEN de-registers the tab from tabs.json. That is the
//! persist-gate — the exact discipline of task's [`perform_claim`]: the close is
//! honoured ONLY when the archive re-read is non-empty AND complete. A failed or
//! empty read-back → NO close + a noisy "RETIRE INCOMPLET, tab kept" flag, the tab
//! stays. This closes the two ghost root-causes: #1 (the card dies at close) and
//! #2 (the tab resurrects on restart).
//!
//! **Invariant cœur** (verrouillé): `close` runs ONLY if the catalog entry, RE-READ,
//! is non-empty and carries {card, session-id}. See [`CatalogCard::is_complete`].
//!
//! **Delivery / safety**: the `shutdown` seam — the ONLY effect that touches a real
//! tab — is INJECTED into [`perform_retire`], so tests (and the future retire
//! script's `--selftest`) record the call + ORDER without closing anything. The
//! order is write-catalog → read-back → [gate] → de-register → shutdown; `shutdown`
//! runs LAST, after the durable de-register, so a failure can never leave the
//! forbidden {closed BUT still in tabs.json} (ghost #2). `self_announce` and the
//! restore chain are untouched.
//!
//! Storage reuses the fabric jsonl convention (append-only under the daemon
//! single-writer lock, like task push): id record = the tab uuid, FIFO = append
//! order, latest-per-id wins on read-back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A retired agent's CARD — a verbatim COPY of the durable `build_snapshot` fields
/// (no new schema): the exact set that survives a restart on `TabState`.
///
/// `id` (the tab uuid) is the record id; `retired_at` stamps the archive. Every
/// field mirrors the card on `TabState`/`SnapshotTab`, so the round-trip is
/// byte-complete (RB1 acceptance 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCard {
    /// The tab uuid — the record id.
    pub id: String,
    /// The canonical catalogue KEY (RC1): `kebab(<role>[-<domain>])` derived from
    /// the card (assignment role + specialty), suffix-free. Stable + reusable
    /// across N instances of a profile — the fold-by-slug key (RB2). Distinct from
    /// `id` (the record) and `name` (freeform). See [`canonical_slug`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specialty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current_task_log: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conventions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluations: Vec<crate::Evaluation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// The archived agent session id — the invariant de réversibilité (RB4
    /// `--resume`). `None` = a session-less agent (legit, fresh-only); losing an
    /// EXISTING session is a bug the persist-gate refuses (see `is_complete`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The archived agent kind (`claude`/`catbus`/…) — paired with `session_id` to
    /// rebuild a `--resume` command (RB4). `None` for a session-less profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// RB3 GATE #1 — the AFTER-ACTION (`set-last-mission`), the agent's closing
    /// summary written at `handoff-written` and archived with the card BEFORE the
    /// close. The one field new to RB3. `None` when the agent posted no after-action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mission: Option<String>,
    /// Unix-millis the card was archived (retired).
    pub retired_at: u64,
}

impl CatalogCard {
    /// Copy the durable card off a persisted [`crate::TabState`] at retire time —
    /// the "copy of the `build_snapshot` card DTO" (zero new serialization: same
    /// fields, same types).
    ///
    /// `after_action` is the RB3 GATE #1 `lastMission`: the agent's closing summary,
    /// captured in the after-action flow (the retire script, at `handoff-written`)
    /// and stamped on the card so it's archived BEFORE the close. A retire-time
    /// input, not a persisted card field — same guarantee, no state plumbing.
    #[must_use]
    pub fn from_tab_state(t: &crate::TabState, after_action: Option<String>, retired_at: u64) -> Self {
        Self {
            id: t.id.clone(),
            // RC1: the canonical slug is derived from the CARD (role + specialty),
            // not the freeform name or the tab-id.
            slug: card_slug(t.assignment.as_deref(), t.specialty.as_deref()),
            name: Some(t.name.clone()).filter(|s| !s.is_empty()),
            assignment: t.assignment.clone(),
            specialty: t.specialty.clone(),
            orchestrator: t.orchestrator.clone(),
            objective: t.objective.clone(),
            current_task_log: t.current_task.clone(),
            conventions: t.conventions.clone(),
            evaluations: t.evaluations.clone(),
            usage_count: t.usage_count,
            last_used_at: t.last_used_at,
            session_id: t.agent_session_id.clone(),
            agent_kind: t.agent_kind.clone(),
            last_mission: after_action.filter(|s| !s.trim().is_empty()),
            retired_at,
        }
    }

    /// The persist-gate predicate: is this RE-READ archive complete enough to close?
    ///
    /// The record must be non-empty (a real `id` = the card is there) AND, WHEN the
    /// tab carried a live session (`had_session`), it must carry the archived
    /// `session_id` — a lost existing session is an incomplete archive. A
    /// session-less agent legitimately has `None` (fresh-only respawn).
    #[must_use]
    pub const fn is_complete(&self, had_session: bool) -> bool {
        !self.id.is_empty() && (!had_session || self.session_id.is_some())
    }
}

// ---------------------------------------------------------------------------
// RC1 — canonical slug at the catalogue write.
//
// The slug is a stable, reusable KEY for a PROFILE (one template, N instances),
// derived from the CARD (assignment role + specialty), NOT the freeform name nor
// the tab-id. Suffix-free by construction (the analogue of bash
// `clean_rehome_name`), so an already-suffixed source never double-suffixes.
// ---------------------------------------------------------------------------

/// Kebab-case one component: ASCII-lowercase, every run of non-alphanumerics → a
/// single `-`, leading/trailing `-` trimmed.
fn kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true; // seeds true → leading separators are trimmed
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Strip a trailing instance suffix `-<n>` (all-digits) so the slug is suffix-free
/// by construction — the numeric analogue of `clean_rehome_name`. Idempotent on
/// stacked suffixes (`builder-2-3` → `builder`).
fn strip_instance_suffix(mut s: &str) -> &str {
    while let Some((head, tail)) = s.rsplit_once('-') {
        if !tail.is_empty() && !head.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            s = head;
        } else {
            break;
        }
    }
    s
}

/// The canonical catalogue SLUG for a card: `kebab(<role>[-<domain>])`, suffix-free.
///
/// `role` (from the assignment, via `role_of`) is the base; `domain` (from the
/// specialty) refines it when present. An empty role falls back to `"agent"`. The
/// role component is stripped of any instance suffix so `builder-2` → `builder`.
#[must_use]
pub fn canonical_slug(role: &str, domain: Option<&str>) -> String {
    let role_k = strip_instance_suffix(&kebab(role)).to_string();
    let role_k = if role_k.is_empty() { "agent".to_string() } else { role_k };
    match domain.map(kebab).filter(|d| !d.is_empty()) {
        Some(d) => format!("{role_k}-{d}"),
        None => role_k,
    }
}

/// The RC1 write key: derive the slug straight off a card's assignment + specialty
/// (role from `role_of(assignment)`, domain from the specialty).
#[must_use]
pub fn card_slug(assignment: Option<&str>, specialty: Option<&str>) -> String {
    canonical_slug(&crate::api::role_of(assignment), specialty)
}

/// Number a LIVE instance of `slug`, suffix-free.
///
/// The bare `<slug>` when free among `existing`, else the smallest free
/// `<slug>-<n>` (n ≥ 2). Distinguishes instances of the same profile without ever
/// double-suffixing (the base is already suffix-free).
#[must_use]
pub fn next_instance(slug: &str, existing: &[String]) -> String {
    if !existing.iter().any(|e| e == slug) {
        return slug.to_string();
    }
    (2u32..=u32::MAX)
        .map(|n| format!("{slug}-{n}"))
        .find(|cand| !existing.iter().any(|e| e == cand))
        .unwrap_or_else(|| slug.to_string())
}

/// Fold a catalogue to ONE row per slug: the LATEST `retired_at` wins, and
/// `usage_count` is AGGREGATED (summed) across every retirement of that profile.
///
/// N retraits of the same profile collapse to a single presentation row (RB2's
/// read-model consumes this) instead of spamming the list. Rows are returned
/// sorted by slug for a stable presentation.
#[must_use]
pub fn dedup_by_slug(cards: &[CatalogCard]) -> Vec<CatalogCard> {
    use std::collections::BTreeMap;
    let mut latest: BTreeMap<String, CatalogCard> = BTreeMap::new();
    let mut usage_total: BTreeMap<String, u64> = BTreeMap::new();
    for c in cards {
        *usage_total.entry(c.slug.clone()).or_default() += c.usage_count.unwrap_or(0);
        match latest.get(&c.slug) {
            Some(prev) if prev.retired_at >= c.retired_at => {}
            _ => {
                latest.insert(c.slug.clone(), c.clone());
            }
        }
    }
    latest
        .into_values()
        .map(|mut c| {
            let total = usage_total[&c.slug];
            c.usage_count = (total > 0).then_some(total);
            c
        })
        .collect()
}

/// The catalogue file: `<state>/tab-atelier/catalog.jsonl`. Honors
/// `TAB_ATELIER_CATALOG_PATH` (a test/ops seam for a full path override).
#[must_use]
pub fn catalog_path() -> PathBuf {
    if let Ok(p) = std::env::var("TAB_ATELIER_CATALOG_PATH") {
        return PathBuf::from(p);
    }
    crate::platform::state_base_dir()
        .join("tab-atelier")
        .join("catalog.jsonl")
}

/// One catalog record as a JSONL line (trailing newline included).
#[must_use]
pub fn encode_catalog_line(card: &CatalogCard) -> String {
    serde_json::to_string(card).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// Parse a catalogue file body into records, skipping blank / unparseable lines
/// (a half-written line from a racing appender is dropped, not fatal — same
/// tolerance as the task queue's `parse_tasks`).
#[must_use]
pub fn parse_catalog(body: &str) -> Vec<CatalogCard> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CatalogCard>(l).ok())
        .collect()
}

/// Append one card to the catalogue (create + append, line-atomic like the swamp /
/// task producer). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / write I/O error.
pub fn append_catalog_line(path: &Path, card: &CatalogCard) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_catalog_line(card).as_bytes())
}

/// RE-READ the catalogue for the LATEST archived card of `id`.
///
/// Append-only → last write wins; `None` when the id was never archived. This is
/// the read-back the persist-gate keys on — proof the write landed, not "I wrote
/// it".
#[must_use]
pub fn read_back(path: &Path, id: &str) -> Option<CatalogCard> {
    let body = std::fs::read_to_string(path).ok()?;
    parse_catalog(&body).into_iter().rev().find(|c| c.id == id)
}

/// The RETIRED read-model (RB2) over a catalogue file.
///
/// Every archived card, folded latest-per-slug + usageCount aggregated (RC1).
/// Path-injectable; a missing file reads as empty. READ-ONLY — a retired card is
/// INERT (only the card fields; no lease/status/claimed@peer, the S4 discipline).
/// Nothing is written or compacted.
#[must_use]
pub fn read_retired_at(path: &Path) -> Vec<CatalogCard> {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    dedup_by_slug(&parse_catalog(&body))
}

/// [`read_retired_at`] against the live [`catalog_path`] — the `retired` section of
/// `GET /dashboard/state` + `tab-atelier catalog list` (RB2). READ-ONLY.
#[must_use]
pub fn read_retired() -> Vec<CatalogCard> {
    read_retired_at(&catalog_path())
}

// ---------------------------------------------------------------------------
// RB4 — spawn --from-card <id|slug>: re-seed a card from the catalogue, closing
// the loop catalogue → spawn → work → retire → catalogue.
// ---------------------------------------------------------------------------

/// Resolve a catalogue card by KEY — an exact `id` first, then the canonical
/// `slug` (latest, since [`read_retired`] already folded latest-per-slug). `None`
/// when neither matches.
#[must_use]
pub fn resolve_card<'a>(cards: &'a [CatalogCard], key: &str) -> Option<&'a CatalogCard> {
    cards
        .iter()
        .find(|c| c.id == key)
        .or_else(|| cards.iter().find(|c| c.slug == key))
}

/// How a `--from-card` spawn brings the agent back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeMode {
    /// FRESH + inject memory — the DEFAULT and the dead-session FALLBACK: clean,
    /// light, zero-risk. The common path.
    Fresh,
    /// Opt-in `--resume`: reattach the archived session via its rebuilt command.
    /// Only when the session is present AND alive; otherwise we fall back to
    /// [`Fresh`](ResumeMode::Fresh) — a `--resume` NEVER blocks or errors a spawn.
    Resume { command: String },
}

/// The re-seed plan for `spawn --from-card` (RB4): the card fields to re-post on
/// the new tab + the bumped usage count + the resume decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReseedPlan {
    pub specialty: Option<String>,
    pub assignment: Option<String>,
    pub conventions: Vec<String>,
    pub objective: Option<String>,
    /// The profile's usage count, BUMPED by this spawn (RB4 acceptance 2).
    pub usage_count: u64,
    pub resume: ResumeMode,
}

/// The resume decision for a `--from-card` spawn (RB4).
///
/// DEFAULT (`want_resume == false`) → [`ResumeMode::Fresh`] (+ inject memory).
/// `--resume` reattaches the archived session ONLY when it's present AND `alive`
/// AND a resume command can be rebuilt ([`crate::restore_resume_command`]);
/// otherwise it FALLS BACK to `Fresh` — a dead/absent session never errors a spawn.
#[must_use]
pub fn resume_mode(card: &CatalogCard, want_resume: bool, session_alive: bool) -> ResumeMode {
    if !want_resume || !session_alive {
        return ResumeMode::Fresh;
    }
    // Opt-in + alive: rebuild the resume command from the archived kind+sid; a
    // session-carrying kind with no usable session falls back to fresh.
    crate::restore_resume_command(card.agent_kind.as_deref(), card.session_id.as_deref(), None)
        .map_or(ResumeMode::Fresh, |command| ResumeMode::Resume { command })
}

/// Build the [`ReseedPlan`] for `spawn --from-card` (RB4): re-post the 4 card
/// fields, bump the usage count, and decide resume-vs-fresh (fallback-safe).
#[must_use]
pub fn reseed_plan(card: &CatalogCard, want_resume: bool, session_alive: bool) -> ReseedPlan {
    ReseedPlan {
        specialty: card.specialty.clone(),
        assignment: card.assignment.clone(),
        conventions: card.conventions.clone(),
        objective: card.objective.clone(),
        usage_count: card.usage_count.unwrap_or(0).saturating_add(1),
        resume: resume_mode(card, want_resume, session_alive),
    }
}

/// Remove tab `id` from a loaded [`crate::SavedState`] — the pure de-register.
///
/// After this, a restore loop iterating `saved.tabs` never sees the id → no tab,
/// no `restore_resume_command` for it (ghost #2 closed). Keeps `active` in range.
/// Returns whether the id was present.
pub fn remove_tab_from_saved(saved: &mut crate::SavedState, id: &str) -> bool {
    let before = saved.tabs.len();
    saved.tabs.retain(|t| t.id != id);
    let removed = saved.tabs.len() != before;
    if removed && saved.active >= saved.tabs.len() && !saved.tabs.is_empty() {
        saved.active = saved.tabs.len() - 1;
    }
    removed
}

/// The REAL de-register seam (RB3 GATE #2): ATOMICALLY persist tabs.json without `id`.
///
/// Load → [`remove_tab_from_saved`] → atomic save (tmp + rename + fsync, via
/// [`crate::save_state_serialized`]). The caller runs this under the daemon
/// single-writer lock and BEFORE the irreversible close, so the crash window
/// RB1-A3 is closed: a crash can never leave the forbidden {closed BUT still in
/// tabs.json}. `Err(NotFound)` when the id wasn't registered (nothing to close).
///
/// # Errors
/// `NotFound` if no tabs.json / the id isn't in it (idempotent — nothing to do).
pub fn deregister_atomic(config_base: &Path, id: &str) -> std::io::Result<()> {
    let Some(mut saved) = crate::load_state_from(config_base) else {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no tabs.json"));
    };
    if !remove_tab_from_saved(&mut saved, id) {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "tab not registered"));
    }
    let json = serde_json::to_string_pretty(&saved).unwrap_or_default();
    crate::save_state_serialized(config_base, &json);
    Ok(())
}

/// The verdict of a retire attempt (RB1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// Archived (read-back OK + complete), de-registered, and closed. Terminal.
    Retired,
    /// The archive write or its read-back was empty / incomplete → close was NEVER
    /// reached, the tab is KEPT (the noisy "RETIRE INCOMPLET" flag). Re-retire is
    /// replayable (idempotent up to the close).
    Incomplete(&'static str),
    /// Archive OK but a terminal step (de-register or shutdown) failed → the tab is
    /// KEPT and NOT a ghost: `shutdown` is the LAST effect, after the durable
    /// de-register, so the forbidden {closed BUT still in tabs.json} can't happen.
    /// Re-retire is replayable.
    CloseFailed,
}

/// The retire window (RB1): archive the card, RE-READ it, and — only on a complete
/// read-back — de-register then close. PURE + persist-gated + fully mockable.
///
/// Mirrors [`crate::cli::task::perform_claim`]: every effect is an injected closure
/// so tests record the call + ORDER without touching a real tab. The persist-gate
/// (the verrouillé invariant) is the `read_back` + [`CatalogCard::is_complete`]
/// check between the archive and the close — a failed/empty read-back means the
/// close is NEVER reached.
///
/// Order of effects: `write_catalog` → `read_back` → [gate] → `deregister` →
/// `shutdown`. `shutdown` (the ONLY real-tab-touching seam) runs LAST, after the
/// durable de-register, so a failure never leaves {closed BUT still registered}.
pub fn perform_retire<Wc, Rb, Dr, Sd>(
    card: &CatalogCard,
    ack_safe_to_close: bool,
    had_session: bool,
    write_catalog: Wc,
    read_back: Rb,
    deregister: Dr,
    shutdown: Sd,
) -> RetireOutcome
where
    Wc: FnOnce(&CatalogCard) -> std::io::Result<()>,
    Rb: FnOnce(&str) -> Option<CatalogCard>,
    Dr: FnOnce() -> std::io::Result<()>,
    Sd: FnOnce() -> std::io::Result<()>,
{
    // GATE 3a (RB3 fail-safe, cumulative + INDEPENDENT of the archive gate): no
    // `safe-to-close` ACK → NO close, the tab is kept. The daemon is the fence,
    // not just the script.
    if !ack_safe_to_close {
        return RetireOutcome::Incomplete("no safe-to-close ACK — RETIRE INCOMPLET, tab kept");
    }
    // 1. Archive the card.
    if write_catalog(card).is_err() {
        return RetireOutcome::Incomplete("archive write failed — tab kept");
    }
    // 2. READ-BACK (proof, not "I wrote it") + gate on completeness. The invariant
    //    cœur: no close unless the re-read archive is non-empty and complete.
    match read_back(&card.id) {
        Some(rb) if rb.is_complete(had_session) => {}
        _ => return RetireOutcome::Incomplete("archive read-back empty/incomplete — RETIRE INCOMPLET, tab kept"),
    }
    // 3. De-register from tabs.json (durable) BEFORE the irreversible close.
    if deregister().is_err() {
        return RetireOutcome::CloseFailed;
    }
    // 4. Close the tab — the LAST effect, the only one that touches a real tab.
    if shutdown().is_err() {
        return RetireOutcome::CloseFailed;
    }
    RetireOutcome::Retired
}

// ---------------------------------------------------------------------------
// CLI (thin HTTP client): `catalog list` GETs the RB2 read-model from the daemon.
// ---------------------------------------------------------------------------

/// `tab-atelier catalog <list>` — the retired-agent catalogue CLI (RB2).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("list") {
        run_list()
    } else {
        eprintln!("usage:\n  tab-atelier catalog list");
        2
    }
}

/// `tab-atelier spawn --from-card <id|slug> [--resume]` (RB4).
///
/// Resolve a retired card and EMIT its re-seed plan (the 4 card fields + bumped
/// usageCount + the resume decision) as JSON, for a spawner (spawn-bot.sh) to
/// apply on a fresh tab.
///
/// The DEFAULT is fresh + inject memory; `--resume` reattaches the archived
/// session, with an automatic fall-back to fresh when the session is dead/absent
/// (the spawner verifies liveness and never lets `--resume` block a spawn).
#[must_use]
pub fn spawn_run(args: &[String]) -> i32 {
    let Some(key) = arg_after(args, "--from-card") else {
        eprintln!("usage:\n  tab-atelier spawn --from-card <id|slug> [--resume]");
        return 2;
    };
    let want_resume = args.iter().any(|a| a == "--resume");
    let cards = read_retired();
    let Some(card) = resolve_card(&cards, key) else {
        eprintln!("spawn: no catalogue card matches '{key}' (id or slug)");
        return 1;
    };
    // The spawner verifies session liveness and falls back to fresh; the plan is
    // built optimistically for a requested resume.
    let plan = reseed_plan(card, want_resume, want_resume);
    let resume = match &plan.resume {
        ResumeMode::Fresh => serde_json::json!({ "mode": "fresh" }),
        ResumeMode::Resume { command } => serde_json::json!({ "mode": "resume", "command": command }),
    };
    let out = serde_json::json!({
        "slug": card.slug,
        "id": card.id,
        "specialty": plan.specialty,
        "assignment": plan.assignment,
        "conventions": plan.conventions,
        "objective": plan.objective,
        "usageCount": plan.usage_count,
        "resume": resume,
    });
    println!("{}", serde_json::to_string(&out).unwrap_or_default());
    0
}

/// The value after `flag` in `args` (`--from-card <value>`).
fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// `catalog list` — GET the retired read-model and print it. READ-ONLY.
fn run_list() -> i32 {
    let ep = match crate::cli::share_link::discover_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("catalog list: {e}");
            return 1;
        }
    };
    let mut resp = match crate::cli::share_link::agent()
        .get(format!("{}/catalog/list", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("catalog list: {e}");
            return 1;
        }
    };
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if status == 200 {
        println!("{text}");
        0
    } else {
        eprintln!("catalog list: HTTP {status}: {text}");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const RETIRED_AT: u64 = 1_000_000;

    /// A card-MAXIMALE `TabState` — every durable field populated — for the
    /// byte-complete round-trip.
    fn maximal_tab_state(id: &str) -> crate::TabState {
        crate::TabState {
            id: id.into(),
            name: format!("agent-{id}"),
            assignment: Some("tab-atelier:agent-lifecycle/builder".into()),
            specialty: Some("rust daemon internals".into()),
            orchestrator: Some("orch-uuid-42".into()),
            objective: Some("ship RB1".into()),
            current_task: vec!["step one".into(), "step two".into()],
            conventions: vec!["CONVENTIONS.md".into(), "memory/index.md".into()],
            evaluations: vec![crate::Evaluation {
                evaluator: "Olympe".into(),
                at: 42,
                verdict: "pass".into(),
                ..Default::default()
            }],
            usage_count: Some(7),
            last_used_at: Some(123_456),
            agent_session_id: Some("sess-abc".into()),
            agent_kind: Some("claude".into()),
            ..Default::default()
        }
    }

    /// A unique temp catalogue path with RAII cleanup (never touches the real dir).
    struct TmpCatalog(PathBuf);
    impl TmpCatalog {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("tab-catalog-{}.jsonl", crate::default_tab_id()));
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpCatalog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // RB1 acceptance (1): round-trip byte-complete — retire a card-MAXIMALE tab →
    // EVERY durable field is present + equal in the catalog entry (a dropped field
    // is red). Proven through the real append + read-back path.
    #[test]
    fn round_trip_is_byte_complete() {
        let cat = TmpCatalog::new();
        let ts = maximal_tab_state("tab-max");
        let card = CatalogCard::from_tab_state(&ts, None, RETIRED_AT);
        append_catalog_line(cat.path(), &card).expect("archive");
        let back = read_back(cat.path(), "tab-max").expect("read-back non-empty");
        assert_eq!(back, card, "every durable field round-trips byte-complete");
        // Spot-check the individually-listed durable fields survived.
        assert_eq!(back.assignment.as_deref(), Some("tab-atelier:agent-lifecycle/builder"));
        assert_eq!(back.specialty.as_deref(), Some("rust daemon internals"));
        assert_eq!(back.objective.as_deref(), Some("ship RB1"));
        assert_eq!(back.current_task_log, vec!["step one", "step two"]);
        assert_eq!(back.conventions, vec!["CONVENTIONS.md", "memory/index.md"]);
        assert_eq!(back.evaluations.len(), 1, "the evaluation ring survives");
        assert_eq!(back.usage_count, Some(7));
        assert_eq!(back.last_used_at, Some(123_456));
        assert_eq!(back.session_id.as_deref(), Some("sess-abc"), "session-id archived");
    }

    /// A recorder + mock seams so the pure retire is tested WITHOUT touching a real
    /// tab. `log` captures the call ORDER; `archived` captures what was written.
    struct Mocks {
        log: RefCell<Vec<&'static str>>,
        archived: RefCell<Option<CatalogCard>>,
    }
    impl Mocks {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                archived: RefCell::new(None),
            }
        }
    }

    fn card(id: &str, session: Option<&str>) -> CatalogCard {
        let assignment = Some("x/builder".to_string());
        CatalogCard {
            id: id.into(),
            slug: card_slug(assignment.as_deref(), None),
            name: None,
            assignment,
            specialty: None,
            orchestrator: None,
            objective: None,
            current_task_log: vec![],
            conventions: vec![],
            evaluations: vec![],
            usage_count: None,
            last_used_at: None,
            session_id: session.map(str::to_string),
            agent_kind: None,
            last_mission: None,
            retired_at: RETIRED_AT,
        }
    }

    // RB1 acceptance (5) RED#2 + (2c): the recorded ORDER is [write-catalog,
    // read-back, de-register, close], and `shutdown` (the real-tab-touching seam)
    // runs LAST — never before the durable de-register.
    #[test]
    fn persist_gate_orders_write_readback_deregister_then_close() {
        let m = Mocks::new();
        let c = card("t1", Some("sess-1"));
        let out = perform_retire(
            &c,
            true, // ack safe-to-close (RB3 gate 3a)
            true,
            |card| {
                m.log.borrow_mut().push("write");
                *m.archived.borrow_mut() = Some(card.clone());
                Ok(())
            },
            |_id| {
                m.log.borrow_mut().push("read-back");
                m.archived.borrow().clone() // proof it was written, re-read
            },
            || {
                m.log.borrow_mut().push("deregister");
                Ok(())
            },
            || {
                m.log.borrow_mut().push("shutdown");
                Ok(())
            },
        );
        assert_eq!(out, RetireOutcome::Retired);
        assert_eq!(
            *m.log.borrow(),
            vec!["write", "read-back", "deregister", "shutdown"],
            "close (shutdown) runs LAST, only after write→read-back→de-register"
        );
    }

    // RB1 acceptance (5) RED#1: a FAILED/empty read-back → close is NEVER called +
    // the tab is kept (the persist-gate). Even though the write "succeeded", the
    // read-back proves nothing landed → no de-register, no shutdown.
    #[test]
    fn persist_gate_empty_read_back_never_closes() {
        let m = Mocks::new();
        let c = card("t1", Some("sess-1"));
        let out = perform_retire(
            &c,
            true, // ack safe-to-close (RB3 gate 3a)
            true,
            |_card| {
                m.log.borrow_mut().push("write");
                Ok(()) // write "ok" but…
            },
            |_id| {
                m.log.borrow_mut().push("read-back");
                None // …read-back comes back EMPTY → gate must refuse
            },
            || {
                m.log.borrow_mut().push("deregister");
                Ok(())
            },
            || {
                m.log.borrow_mut().push("shutdown");
                Ok(())
            },
        );
        assert!(matches!(out, RetireOutcome::Incomplete(_)), "empty read-back → Incomplete");
        assert_eq!(
            *m.log.borrow(),
            vec!["write", "read-back"],
            "no de-register, NO close — the tab is kept (RETIRE INCOMPLET)"
        );
    }

    // RB1 acceptance (6): session-id archived BEFORE close. A tab WITH a session →
    // the written card carries the session_id (captured at write, before any close);
    // and losing an existing session (had_session but None archived) fails the gate.
    #[test]
    fn session_id_is_archived_before_close() {
        let m = Mocks::new();
        let c = card("t1", Some("sess-xyz"));
        let out = perform_retire(
            &c,
            true, // ack safe-to-close (RB3 gate 3a)
            true,
            |card| {
                *m.archived.borrow_mut() = Some(card.clone());
                Ok(())
            },
            |_id| m.archived.borrow().clone(),
            || Ok(()),
            || {
                // At close time, the archive already carries the session-id.
                let archived = m.archived.borrow();
                assert_eq!(
                    archived.as_ref().and_then(|a| a.session_id.as_deref()),
                    Some("sess-xyz"),
                    "session-id is in the catalog BEFORE the close"
                );
                Ok(())
            },
        );
        assert_eq!(out, RetireOutcome::Retired);

        // A lost EXISTING session (had_session=true, but archived session None) →
        // the gate refuses (incomplete archive), no close.
        let lost = card("t2", None);
        let out2 = perform_retire(
            &lost,
            true, // ack safe-to-close (RB3 gate 3a)
            true, // the tab HAD a session…
            |_c| Ok(()),
            |_id| Some(lost.clone()), // …but the archive lost it → None
            || Ok(()),
            || panic!("must not close: an existing session was lost"),
        );
        assert!(matches!(out2, RetireOutcome::Incomplete(_)), "lost session → gate refuses");

        // A genuinely session-less agent (had_session=false, None) is legit.
        let sessionless = card("t3", None);
        let out3 = perform_retire(
            &sessionless,
            true, // ack safe-to-close (RB3 gate 3a)
            false,
            |_c| Ok(()),
            |_id| Some(sessionless.clone()),
            || Ok(()),
            || Ok(()),
        );
        assert_eq!(out3, RetireOutcome::Retired, "session-less None is legit → closes");
    }

    // RB1 acceptance (7): idempotence up to the close. A failure at ANY pre-close
    // step → the tab stays (no shutdown), re-retire replayable. Two injected faults.
    #[test]
    fn idempotent_until_close_on_any_pre_close_failure() {
        // (a) archive write fails → nothing else runs.
        let m = Mocks::new();
        let c = card("t1", None);
        let out = perform_retire(
            &c,
            true, // ack safe-to-close (RB3 gate 3a)
            false,
            |_c| Err(std::io::Error::other("disk full")),
            |_id| {
                m.log.borrow_mut().push("read-back");
                Some(c.clone())
            },
            || {
                m.log.borrow_mut().push("deregister");
                Ok(())
            },
            || {
                m.log.borrow_mut().push("shutdown");
                Ok(())
            },
        );
        assert!(matches!(out, RetireOutcome::Incomplete(_)));
        assert!(m.log.borrow().is_empty(), "write failed → no read-back, no de-register, NO close");

        // (b) de-register fails (after a good read-back) → shutdown NEVER runs, so
        // the tab is never {closed AND still registered} (ghost #2 impossible).
        let m2 = Mocks::new();
        let c2 = card("t2", None);
        let out2 = perform_retire(
            &c2,
            true, // ack safe-to-close (RB3 gate 3a)
            false,
            |_c| Ok(()),
            |_id| Some(c2.clone()),
            || Err(std::io::Error::other("rename failed")),
            || {
                m2.log.borrow_mut().push("shutdown");
                Ok(())
            },
        );
        assert_eq!(out2, RetireOutcome::CloseFailed);
        assert!(m2.log.borrow().is_empty(), "de-register failed → shutdown NEVER runs (no ghost)");
    }

    // RB1 acceptance (8): non-régression voisin — retiring one tab doesn't touch
    // another's card. Two cards archived; retiring/reading X leaves Y intact, and
    // de-registering X leaves Y in tabs.json.
    #[test]
    fn retire_does_not_touch_a_neighbours_card() {
        let cat = TmpCatalog::new();
        let x = CatalogCard::from_tab_state(&maximal_tab_state("tab-x"), None, RETIRED_AT);
        let y = CatalogCard::from_tab_state(&maximal_tab_state("tab-y"), None, RETIRED_AT);
        append_catalog_line(cat.path(), &x).unwrap();
        append_catalog_line(cat.path(), &y).unwrap();
        // Read-back of Y is intact after X was archived alongside it.
        assert_eq!(read_back(cat.path(), "tab-y"), Some(y), "neighbour Y's card is untouched");

        // De-registering X from a two-tab SavedState leaves Y registered.
        let mut saved = crate::SavedState {
            tabs: vec![maximal_tab_state("tab-x"), maximal_tab_state("tab-y")],
            active: 1,
            windowed: false,
            dashboard_share_token: String::new(),
        };
        assert!(remove_tab_from_saved(&mut saved, "tab-x"), "X removed");
        assert_eq!(saved.tabs.len(), 1);
        assert_eq!(saved.tabs[0].id, "tab-y", "Y stays registered");
        assert_eq!(saved.active, 0, "active clamped into range");
    }

    // RB1 acceptance (2a)+(2b): de-register → the id is absent from tabs.json AND a
    // restore loop over the saved tabs never sees it (no tab, no resume_command).
    #[test]
    fn deregistered_id_is_absent_and_never_restored() {
        let mut saved = crate::SavedState {
            tabs: vec![maximal_tab_state("ghost"), maximal_tab_state("keep")],
            active: 0,
            windowed: false,
            dashboard_share_token: String::new(),
        };
        assert!(remove_tab_from_saved(&mut saved, "ghost"));
        // (2a) absent from the persisted set.
        assert!(!saved.tabs.iter().any(|t| t.id == "ghost"), "id absent from tabs.json");
        // (2b) the restore loop iterates saved.tabs → it can't restore nor build a
        // resume_command for an id that isn't there.
        let restored_ids: Vec<&str> = saved.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(restored_ids, vec!["keep"], "restore loop never sees the de-registered id");
        // A second de-register is a no-op (idempotent), never resurrecting it.
        assert!(!remove_tab_from_saved(&mut saved, "ghost"), "re-de-register is a no-op");
    }

    // RB1 acceptance (2c) — atomicity close↔de-register, seen from the terminal
    // order: a persist() replay after a completed retire must NOT re-add the id, and
    // `shutdown` only ever runs after the de-register (proven in the order test).
    #[test]
    fn a_completed_retire_does_not_re_add_the_id() {
        let mut saved = crate::SavedState {
            tabs: vec![maximal_tab_state("gone")],
            active: 0,
            windowed: false,
            dashboard_share_token: String::new(),
        };
        // Retire de-registers it.
        assert!(remove_tab_from_saved(&mut saved, "gone"));
        assert!(saved.tabs.is_empty());
        // A later persist() rebuilds tabs.json from the (now shorter) set — it can't
        // re-add an id that's no longer in the runtime list.
        assert!(!saved.tabs.iter().any(|t| t.id == "gone"), "persist replay never re-adds a retired id");
    }

    #[test]
    fn parse_catalog_skips_blank_and_garbage_and_latest_wins() {
        let body = "\n  \nnot json\n{\"id\":\"a\",\"retiredAt\":1}\n{\"id\":\"a\",\"retiredAt\":2,\"specialty\":\"newer\"}\n";
        let cards = parse_catalog(body);
        assert_eq!(cards.len(), 2, "blank + garbage skipped");
        // read_back over a written file returns the LATEST for the id.
        let cat = TmpCatalog::new();
        std::fs::write(cat.path(), body).unwrap();
        let back = read_back(cat.path(), "a").expect("found");
        assert_eq!(back.retired_at, 2, "latest append wins on read-back");
        assert_eq!(back.specialty.as_deref(), Some("newer"));
    }

    // ----- RC1: canonical slug at the catalogue write ---------------------------

    // RC1: the slug is kebab(<role>[-<domain>]), suffix-free — role alone,
    // role+domain, an already-suffixed source stripped, empty→agent.
    #[test]
    fn rc1_canonical_slug_is_kebab_role_domain_suffix_free() {
        // role alone.
        assert_eq!(canonical_slug("builder", None), "builder");
        assert_eq!(canonical_slug("Code Reviewer", None), "code-reviewer", "kebab + lowercase");
        // role + domain (from specialty).
        assert_eq!(canonical_slug("builder", Some("rust daemon")), "builder-rust-daemon");
        assert_eq!(
            canonical_slug("reviewer", Some("SQL / DB")),
            "reviewer-sql-db",
            "non-alnum runs collapse to one dash"
        );
        // an already-suffixed source is stripped (suffix-free by construction).
        assert_eq!(canonical_slug("builder-2", None), "builder", "instance suffix stripped");
        assert_eq!(canonical_slug("builder-2-3", None), "builder", "stacked suffixes stripped");
        // empty role → the "agent" fallback (never an empty slug).
        assert_eq!(canonical_slug("", None), "agent");
        assert_eq!(canonical_slug("  ", Some("payments")), "agent-payments");
    }

    // RC1: derived straight off a card's assignment (role) + specialty (domain).
    #[test]
    fn rc1_card_slug_derives_from_assignment_and_specialty() {
        assert_eq!(card_slug(Some("build/builder"), None), "builder", "role from the assignment");
        assert_eq!(
            card_slug(Some("kalpin-back:review/reviewer"), Some("postgres")),
            "reviewer-postgres",
            "override ignored, role + specialty domain"
        );
        assert_eq!(card_slug(None, None), "agent", "no assignment → agent");
        // The slug lands on the card at write time (from_tab_state).
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        assert_eq!(card.slug, "builder-rust-daemon-internals", "slug computed from the card");
    }

    // RC1: live instances of a profile are numbered <slug>-<n>, suffix-free —
    // the bare slug when free, else the smallest free -<n> (n ≥ 2).
    #[test]
    fn rc1_next_instance_numbers_collisions() {
        assert_eq!(next_instance("builder", &[]), "builder", "free → bare slug");
        let existing = vec!["builder".to_string()];
        assert_eq!(next_instance("builder", &existing), "builder-2", "first collision → -2");
        let existing = vec!["builder".to_string(), "builder-2".to_string()];
        assert_eq!(next_instance("builder", &existing), "builder-3", "next free -n");
        // A gap is filled by the smallest free index, never double-suffixing.
        let existing = vec!["builder".to_string(), "builder-3".to_string()];
        assert_eq!(next_instance("builder", &existing), "builder-2", "smallest free index");
    }

    // RC1: dedup+aggregate by slug — N retraits of a profile collapse to ONE row
    // (latest retired_at wins), usageCount summed. A profile doesn't spam the list.
    #[test]
    fn rc1_dedup_by_slug_folds_latest_and_aggregates_usage() {
        let mk = |id: &str, slug: &str, retired: u64, usage: u64| CatalogCard {
            id: id.into(),
            slug: slug.into(),
            usage_count: Some(usage),
            objective: Some(format!("obj-{id}")),
            retired_at: retired,
            name: None,
            assignment: None,
            specialty: None,
            orchestrator: None,
            current_task_log: vec![],
            conventions: vec![],
            evaluations: vec![],
            last_used_at: None,
            session_id: None,
            agent_kind: None,
            last_mission: None,
        };
        let cards = vec![
            mk("id1", "builder", 100, 3), // older builder
            mk("id2", "builder", 200, 5), // newer builder → wins
            mk("id3", "reviewer", 150, 2),
        ];
        let folded = dedup_by_slug(&cards);
        assert_eq!(folded.len(), 2, "two profiles → two rows (builder folded)");
        let builder = folded.iter().find(|c| c.slug == "builder").expect("builder row");
        assert_eq!(builder.id, "id2", "the LATEST retirement wins the row");
        assert_eq!(builder.objective.as_deref(), Some("obj-id2"), "latest card's fields");
        assert_eq!(builder.usage_count, Some(8), "usageCount aggregated across retraits (3+5)");
        let reviewer = folded.iter().find(|c| c.slug == "reviewer").expect("reviewer row");
        assert_eq!(reviewer.usage_count, Some(2));
    }

    // ----- RB2: the retired read-model -----------------------------------------

    // RB2: read_retired folds the catalogue latest-per-slug (N retraits of a
    // profile → ONE row, no spam), keeps every card field, aggregates usageCount,
    // and is READ-ONLY / INERT — a retired card carries NO lease/status/claimed@peer.
    #[test]
    fn rb2_read_retired_folds_by_slug_keeps_fields_and_is_inert() {
        let cat = TmpCatalog::new();
        // two retraits of the SAME profile (builder) + one reviewer.
        let mut b1 = CatalogCard::from_tab_state(&maximal_tab_state("id1"), None, 100);
        b1.usage_count = Some(3);
        let mut b2 = CatalogCard::from_tab_state(&maximal_tab_state("id2"), None, 200);
        b2.usage_count = Some(5);
        let mut rev = CatalogCard::from_tab_state(&maximal_tab_state("id3"), None, 150);
        rev.assignment = Some("x/reviewer".into());
        rev.specialty = None;
        rev.slug = card_slug(rev.assignment.as_deref(), rev.specialty.as_deref());
        append_catalog_line(cat.path(), &b1).unwrap();
        append_catalog_line(cat.path(), &b2).unwrap();
        append_catalog_line(cat.path(), &rev).unwrap();

        let retired = read_retired_at(cat.path());
        assert_eq!(retired.len(), 2, "N retraits of a profile fold to ONE row (no spam)");
        let builder = retired
            .iter()
            .find(|c| c.slug == "builder-rust-daemon-internals")
            .expect("builder profile present");
        assert_eq!(builder.id, "id2", "the LATEST retirement wins the row");
        assert_eq!(builder.usage_count, Some(8), "usageCount aggregated (3+5)");
        // Every card field is present on the retired card.
        assert_eq!(builder.assignment.as_deref(), Some("tab-atelier:agent-lifecycle/builder"));
        assert_eq!(builder.objective.as_deref(), Some("ship RB1"));
        assert_eq!(builder.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(builder.evaluations.len(), 1, "the evaluation ring survives");
        // READ-ONLY / INERT: the serialized retired card carries no live-state key.
        let json = serde_json::to_string(builder).unwrap();
        for forbidden in ["lease", "\"status\"", "claimedBy", "\"state\""] {
            assert!(!json.contains(forbidden), "a retired card is inert (no {forbidden}): {json}");
        }
        assert!(retired.iter().any(|c| c.slug == "reviewer"), "the reviewer profile is its own row");
    }

    // ----- RB3: the retire script's daemon-side gates ---------------------------

    // RB3 GATE 3a (fail-safe, INDEPENDENT of the archive gate): no `safe-to-close`
    // ACK → NO close, the tab is kept + the noisy flag. Nothing else even runs.
    #[test]
    fn rb3_gate_3a_no_ack_safe_to_close_never_closes() {
        let m = Mocks::new();
        let c = card("t1", Some("sess-1"));
        let out = perform_retire(
            &c,
            false, // NO safe-to-close ACK (gate 3a)
            true,
            |_c| {
                m.log.borrow_mut().push("write");
                Ok(())
            },
            |_id| {
                m.log.borrow_mut().push("read-back");
                Some(c.clone())
            },
            || {
                m.log.borrow_mut().push("deregister");
                Ok(())
            },
            || {
                m.log.borrow_mut().push("shutdown");
                Ok(())
            },
        );
        assert!(matches!(out, RetireOutcome::Incomplete(_)), "no ACK → Incomplete");
        assert!(m.log.borrow().is_empty(), "no ACK → NOTHING runs (no archive, no close)");
    }

    // RB3: the two gates are CUMULATIVE + INDEPENDENT — each blocks the close on
    // its own. (3a) ACK missing while archive OK → blocked. (3b) archive missing
    // while ACK OK → blocked. Only BOTH present → close.
    #[test]
    fn rb3_gates_3a_and_3b_block_independently() {
        let c = card("t1", None);
        // 3a fails (no ACK), 3b would pass (read-back ok) → blocked, no close.
        let out_no_ack = perform_retire(
            &c,
            false,
            false,
            |_c| Ok(()),
            |_id| Some(c.clone()),
            || Ok(()),
            || panic!("must not close: no ACK"),
        );
        assert!(matches!(out_no_ack, RetireOutcome::Incomplete(_)), "3a blocks");
        // 3a passes (ACK), 3b fails (empty read-back) → blocked, no close.
        let out_no_archive = perform_retire(
            &c,
            true,
            false,
            |_c| Ok(()),
            |_id| None, // read-back empty
            || Ok(()),
            || panic!("must not close: archive not verified"),
        );
        assert!(matches!(out_no_archive, RetireOutcome::Incomplete(_)), "3b blocks");
        // Both present → close.
        let out_ok = perform_retire(&c, true, false, |_c| Ok(()), |_id| Some(c.clone()), || Ok(()), || Ok(()));
        assert_eq!(out_ok, RetireOutcome::Retired, "both gates satisfied → close");
    }

    // RB3 GATE #1: the after-action (lastMission) is stamped on the card and
    // ARCHIVED before the close — captured in the after-action flow, written at
    // archive time (write_catalog), before any de-register/shutdown.
    #[test]
    fn rb3_last_mission_is_archived_before_close() {
        let after = Some("shipped RB3; handed off cleanly".to_string());
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), after.clone(), RETIRED_AT);
        assert_eq!(card.last_mission, after, "the after-action lands on the card");

        let seen_at_write = RefCell::new(None);
        let out = perform_retire(
            &card,
            true,
            true,
            |c| {
                *seen_at_write.borrow_mut() = c.last_mission.clone();
                Ok(())
            },
            |_id| Some(card.clone()),
            || Ok(()),
            || {
                // At close, the archived card already carried the after-action.
                assert_eq!(*seen_at_write.borrow(), after, "lastMission archived BEFORE close");
                Ok(())
            },
        );
        assert_eq!(out, RetireOutcome::Retired);
        // A blank after-action is dropped (no empty lastMission).
        let blank = CatalogCard::from_tab_state(&maximal_tab_state("t"), Some("   ".into()), RETIRED_AT);
        assert!(blank.last_mission.is_none(), "a blank after-action archives as None");
    }

    // RB3 GATE #2 (crash window RB1-A3): `shutdown` runs LAST, after the durable
    // de-register. Injecting a crash AT the close (shutdown Err) can never leave
    // {closed BUT still in tabs.json} — the id was already removed+persisted.
    #[test]
    fn rb3_crash_between_deregister_and_close_never_leaves_a_ghost() {
        let c = card("gone", None);
        // The de-register persists an in-memory "saved state" WITHOUT the id.
        let saved: RefCell<Vec<String>> = RefCell::new(vec!["gone".into(), "keep".into()]);
        let out = perform_retire(
            &c,
            true,
            false,
            |_c| Ok(()),
            |_id| Some(c.clone()),
            || {
                saved.borrow_mut().retain(|id| id != "gone"); // durable remove BEFORE close
                Ok(())
            },
            || Err(std::io::Error::other("CRASH at close")), // the close crashes
        );
        assert_eq!(out, RetireOutcome::CloseFailed, "a crash at close → not Retired");
        // The invariant: the id is ALREADY gone from the persisted state — never a
        // ghost {closed BUT still registered}. Re-retire is replayable.
        assert!(!saved.borrow().contains(&"gone".to_string()), "de-registered BEFORE the crash — no ghost");
        assert!(saved.borrow().contains(&"keep".to_string()), "the neighbour is untouched");
    }

    // RB3 GATE #2 real seam: deregister_atomic loads, removes, and ATOMICALLY
    // re-persists tabs.json (tmp+rename) — the id is gone on reload, neighbours
    // stay; an absent id is a NotFound no-op (idempotent).
    #[test]
    fn rb3_deregister_atomic_persists_removal() {
        // A unique temp config base so we never touch the real state dir.
        let base = std::env::temp_dir().join(format!("tab-dereg-{}", crate::default_tab_id()));
        let _cleanup = scopeguard_remove(&base);
        // Seed a tabs.json with two tabs via the real atomic writer.
        let saved = crate::SavedState {
            tabs: vec![maximal_tab_state("gone"), maximal_tab_state("keep")],
            active: 0,
            windowed: false,
            dashboard_share_token: String::new(),
        };
        crate::save_state_serialized(&base, &serde_json::to_string(&saved).unwrap());

        // De-register "gone" atomically → reload shows it gone, "keep" stays.
        deregister_atomic(&base, "gone").expect("de-register persists");
        let reloaded = crate::load_state_from(&base).expect("reload");
        assert!(!reloaded.tabs.iter().any(|t| t.id == "gone"), "id gone from tabs.json (atomic)");
        assert!(reloaded.tabs.iter().any(|t| t.id == "keep"), "neighbour persisted");
        // Re-de-register is a NotFound no-op (idempotent, never resurrects).
        assert!(deregister_atomic(&base, "gone").is_err(), "absent id → NotFound no-op");
    }

    /// A tiny RAII cleanup for the temp config dir (no external crate).
    fn scopeguard_remove(dir: &Path) -> impl Drop {
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        Rm(dir.to_path_buf())
    }

    // ----- RB4: spawn --from-card -----------------------------------------------

    // RB4: resolve a card by id OR by slug (id wins), None when neither matches.
    #[test]
    fn rb4_resolve_card_by_id_or_slug() {
        let c = CatalogCard::from_tab_state(&maximal_tab_state("the-id"), None, RETIRED_AT);
        let cards = vec![c];
        assert_eq!(resolve_card(&cards, "the-id").map(|c| &c.id), Some(&"the-id".to_string()), "by id");
        assert_eq!(
            resolve_card(&cards, "builder-rust-daemon-internals").map(|c| &c.id),
            Some(&"the-id".to_string()),
            "by slug"
        );
        assert!(resolve_card(&cards, "nope").is_none(), "no match → None");
    }

    // RB4 acceptance (1)+(2): the plan re-seeds the 4 card fields (specialty,
    // assignment, conventions, objective) and BUMPS usageCount.
    #[test]
    fn rb4_reseed_plan_reposts_four_fields_and_bumps_usage() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        // maximal has usage_count Some(7).
        let plan = reseed_plan(&card, false, false);
        assert_eq!(plan.specialty.as_deref(), Some("rust daemon internals"), "specialty re-posted");
        assert_eq!(
            plan.assignment.as_deref(),
            Some("tab-atelier:agent-lifecycle/builder"),
            "assignment re-posted"
        );
        assert_eq!(plan.conventions, vec!["CONVENTIONS.md", "memory/index.md"], "conventions re-posted");
        assert_eq!(plan.objective.as_deref(), Some("ship RB1"), "objective re-posted");
        assert_eq!(plan.usage_count, 8, "usageCount bumped 7 → 8");
    }

    // RB4 acceptance (3): the DEFAULT is fresh + inject memory (no --resume).
    #[test]
    fn rb4_default_is_fresh() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        assert_eq!(
            reseed_plan(&card, false, true).resume,
            ResumeMode::Fresh,
            "default (no --resume) → fresh + memory, even with a live session"
        );
    }

    // RB4 acceptance (4): --resume is opt-in — reattaches the ARCHIVED session via
    // its rebuilt command when present + alive.
    #[test]
    fn rb4_resume_opt_in_reattaches_the_archived_session() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        // maximal has agent_kind "claude" + session "sess-abc".
        let mode = reseed_plan(&card, true, true).resume;
        match mode {
            ResumeMode::Resume { command } => {
                assert!(command.contains("sess-abc"), "the resume command carries the archived session id");
            }
            ResumeMode::Fresh => panic!("--resume with a live archived session must resume"),
        }
    }

    // RB4 acceptance (5) — the CRITICAL red: --resume with a DEAD/absent session →
    // AUTOMATIC fallback to fresh + memory, NEVER a hard error. `--resume` must
    // never block a spawn.
    #[test]
    fn rb4_dead_or_absent_session_falls_back_to_fresh() {
        // (a) session present but DEAD (alive=false) → fresh fallback.
        let live = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        assert_eq!(
            reseed_plan(&live, true, false).resume,
            ResumeMode::Fresh,
            "--resume + dead session → fresh fallback, no error"
        );
        // (b) session-less profile (no session_id) → fresh fallback even with --resume.
        let mut ts = maximal_tab_state("t2");
        ts.agent_session_id = None;
        ts.agent_kind = None;
        let sessionless = CatalogCard::from_tab_state(&ts, None, RETIRED_AT);
        assert_eq!(
            reseed_plan(&sessionless, true, true).resume,
            ResumeMode::Fresh,
            "--resume + no archived session → fresh fallback, no error"
        );
    }
}
