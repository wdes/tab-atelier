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

/// How an instance of a skill was BORN — the per-instance PARTITION key for the
/// v2 metrics (SV3).
///
/// `Origin` is the hand-built genesis instance, EXCLUDED from the fresh-vs-resume
/// A/B; only `Fresh` and `Resume` are the two benched arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpawnMode {
    /// Spawned fresh from the distilled profile (+ task overlay) — the default arm.
    Fresh,
    /// Reattached the baseline session (`--resume`) — the A/B champion arm.
    Resume,
    /// The genesis instance (hand-built, no profile ancestor). Excluded from the A/B.
    Origin,
}

/// The retire OUTCOME — v2. Derived from the éval-à-3 (SV2), NOT a self-report. The
/// default is `Problem` — a conservative bar: nothing is a success until the eval says
/// so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    #[default]
    Problem,
}

/// SC1 (#39) — the catalogue is EVENT-SOURCED: every mutation is an append. `kind`
/// discriminates the record TYPE so the read-model folds on TWO independent axes:
/// - CONTENT (profile) = latest-append-wins over `{Retire, Edit}` records.
/// - VISIBILITY = last-wins over `{Delete, Restore}` records ONLY.
///
/// `Retire` is the default → a record with no `kind` key (every v1 + SV1-SV5 record)
/// reads as a retire, BYTE-IDENTICAL to before, so the v1 quarantine is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// An agent retirement — a metric data-point + a profile snapshot. The default.
    #[default]
    Retire,
    /// A profile EDIT (dashboard) — new prompt/specialty/conventions, `promptVersion++`.
    /// Touches CONTENT, never VISIBILITY.
    Edit,
    /// A tombstone — hides the skill from the read-model (STICKY). Touches VISIBILITY.
    Delete,
    /// An explicit un-tombstone — the ONLY resurrection path. Touches VISIBILITY.
    Restore,
}

impl RecordKind {
    /// A retire record is the default; used to keep it out of the serialized JSON so
    /// existing records stay byte-identical. (`&self` — serde `skip_serializing_if`
    /// requires a `fn(&T)`.)
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_retire(&self) -> bool {
        matches!(self, Self::Retire)
    }
    /// A CONTENT record — carries profile fields (`Retire` or `Edit`).
    const fn is_content(self) -> bool {
        matches!(self, Self::Retire | Self::Edit)
    }
    /// A VISIBILITY record — flips the tombstone (`Delete` or `Restore`).
    const fn is_visibility(self) -> bool {
        matches!(self, Self::Delete | Self::Restore)
    }
}

/// The v2 stamp an orchestrator applies at retire time (SV3).
///
/// The distilled PROFILE fields (`skill` name + prompt/tools/patterns) plus this
/// instance's per-mode telemetry (`spawn_mode`, `outcome`, `tokens`, `cost`).
/// Everything is optional so a legacy (v1) retire — which carries none of it — stays
/// byte-identical. The `specialty`/`conventions` profile fields already live on the
/// card and are reused.
///
/// The proper `skill` NAME (SV5-nom) is produced by the agent's bilan + éval-à-3
/// (SV1/SV2, later slices); this foundation accepts it as an input and falls back to
/// the canonical slug when absent, so a record is always foldable.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V2Stamp {
    pub skill: Option<String>,
    pub prompt_version: Option<u32>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    pub spawn_mode: Option<SpawnMode>,
    pub outcome: Option<Outcome>,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
    pub difficulty: Option<u8>,
}

impl V2Stamp {
    /// Is this a v2 retire? The orchestrator opts in by naming a `skill`; without a
    /// name the retire stays a v1 record (backward-compatible default).
    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.skill.as_ref().is_some_and(|s| !s.trim().is_empty())
    }
}

/// The structured BILAN an agent produces at retire (SV1) — a RETROSPECTIVE ON ITS
/// PROMPT, not on the precise task.
///
/// It replaces the 1-line `lastMission`: instead of "what I did", it captures what
/// the agent learned about its ROLE/PROMPT and how the prompt should change — the raw
/// material for the improved prompt (SV2) and the v2 record's profile (SV3). Every
/// field is GENERALISABLE (about the base prompt/context), never the run's precise
/// facts — those stay in `objective`/`current_task_log`, untouched. Written AT the
/// retire, BEFORE the éval (SV2). All fields optional so a bilan is only as full as
/// the agent made it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bilan {
    /// What the agent LEARNED about its role/prompt (generalisable, not task facts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learned: Vec<String>,
    /// PROBLEMS with the base prompt/context that surfaced this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Directives to ADD to the prompt (+consignes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_directives: Vec<String>,
    /// Directives to REMOVE from the prompt (−consignes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop_directives: Vec<String>,
}

impl Bilan {
    /// A bilan with nothing in any of its four fields — treated as "no bilan".
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.learned.is_empty() && self.problems.is_empty() && self.add_directives.is_empty()
            && self.drop_directives.is_empty()
    }

    /// A compact one-line summary — the back-fill for the legacy `lastMission` slot so
    /// consumers that still read it get a readable digest of the structured bilan.
    #[must_use]
    pub fn one_line(&self) -> String {
        let seg = |label: &str, items: &[String]| {
            (!items.is_empty()).then(|| format!("{label}: {}", items.join("; ")))
        };
        [
            seg("learned", &self.learned),
            seg("problems", &self.problems),
            seg("+prompt", &self.add_directives),
            seg("−prompt", &self.drop_directives),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

// ---------------------------------------------------------------------------
// SV2 — the a-priori ÉVAL-À-3: agent(bilan) + orchestrator + Olympe converge on the
// IMPROVED prompt (consensus) or keep the original (dissent → statu quo). The
// `outcome` is DERIVED from the eval, never self-reported. Étage-1 anti-over-fit is
// ENFORCED here (FN2): a mechanical ban on task LITERALS (E3b) + a test-nouvelle-tâche
// (E3c: no ADDED directive may be task-specific, or the prompt wouldn't hold on a new
// task). The per-directive general|task-specific verdict (FN1) is the EVAL's neutral
// output, NOT part of the SV1 bilan.
// ---------------------------------------------------------------------------

/// One evaluator's vote in the éval-à-3 (agent / orchestrator / Olympe).
///
/// The default (both `false`) is a conservative abstention: a missing vote is a NO on
/// the prompt (→ statu quo) and a NO on the run (→ problem). Silence never improves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalVote {
    /// Approves adopting the improved prompt.
    pub approve_prompt: bool,
    /// Judges the instance's RUN a success (the outcome signal — NOT self-report:
    /// each of the three evaluators votes, the outcome is derived from the majority).
    pub run_ok: bool,
}

/// The three votes of the éval-à-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalVotes {
    pub agent: EvalVote,
    pub orchestrator: EvalVote,
    pub olympe: EvalVote,
}

/// The inputs to the éval-à-3 (SV2), produced at retire before catalogage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalInput {
    /// The base (current) prompt the eval may improve.
    #[serde(default)]
    pub base_prompt: String,
    /// The agent's bilan (SV1) — its `add_directives` are the proposed prompt changes.
    #[serde(default)]
    pub bilan: Bilan,
    /// The PRECISE task's concrete values that must NOT leak into the prompt (FN2 E3b).
    /// The live path fills these from the tab's objective + current-task log.
    #[serde(default)]
    pub task_literals: Vec<String>,
    /// The three evaluators' votes.
    #[serde(default)]
    pub votes: EvalVotes,
}

/// FN1: an ADDED directive's scope — the eval's neutral judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectiveScope {
    /// Generalisable — holds on a new task. Eligible for the improved prompt.
    General,
    /// Task-specific — carries the past run's specifics; would over-fit the prompt.
    TaskSpecific,
}

/// FN1: one added directive tagged with its scope (part of the EVAL output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectiveVerdict {
    pub directive: String,
    pub scope: DirectiveScope,
}

/// The eval decision: adopt the improved prompt, or keep the original.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvalDecision {
    /// Consensus + gates passed → the improved prompt is adopted.
    Improved,
    /// Dissent OR an anti-over-fit veto → the original prompt stands. The default: no
    /// eval means no change.
    #[default]
    StatuQuo,
}

/// The éval-à-3 OUTPUT stored on the v2 record (FN1 verdicts + FN2 findings + the
/// traced decision + the DERIVED outcome). The resulting prompt is applied to the
/// card's `prompt`, not duplicated here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalReport {
    pub decision: EvalDecision,
    /// The outcome DERIVED from the three `run_ok` votes (majority), not self-reported.
    pub outcome: Outcome,
    /// FN1: per-added-directive general|task-specific verdicts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directive_verdicts: Vec<DirectiveVerdict>,
    /// FN2 E3b: task literals that leaked into a proposed directive (a ban trigger).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaked_literals: Vec<String>,
    /// FN2 E3c: added directives that are task-specific (fail the test-nouvelle-tâche).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_specific_directives: Vec<String>,
    /// A human-readable trace of why the decision landed as it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
}

/// The result of the éval-à-3: the [`EvalReport`] to store + the resulting prompt to
/// apply to the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    pub report: EvalReport,
    pub resulting_prompt: String,
}

/// A non-empty, non-whitespace literal is bannable. Punctuation-only / tiny tokens
/// (from splitting) are ignored so the ban targets real task values, not noise.
fn is_meaningful_literal(lit: &str) -> bool {
    lit.trim().chars().filter(|c| c.is_alphanumeric()).count() >= 3
}

/// Run the a-priori éval-à-3 (SV2). PURE.
///
/// Given the base prompt, the agent's bilan, the task's literals, and the three votes,
/// it decides improved-vs-statu-quo, derives the outcome, tags each directive (FN1),
/// and enforces the anti-over-fit gate (FN2).
///
/// - **FN1** tags each ADD directive `General` / `TaskSpecific` — task-specific ⇔ it
///   contains a task literal (a concrete past-run value).
/// - **FN2 E3b (ban literals)**: any task literal appearing in a directive is leaked.
/// - **FN2 E3c (test-nouvelle-tâche)**: any task-specific directive would not hold on a
///   new task.
/// - **Decision**: `Improved` iff all three approve AND no leak AND no task-specific
///   directive; otherwise `StatuQuo` (the veto beats a rubber-stamp consensus).
/// - **Outcome**: DERIVED from the three `run_ok` votes (majority), never self-report.
#[must_use]
pub fn evaluate(input: &EvalInput) -> EvalResult {
    let literals: Vec<&str> = input.task_literals.iter().map(String::as_str).filter(|l| is_meaningful_literal(l)).collect();
    let contains_literal = |text: &str| {
        let hay = text.to_lowercase();
        literals.iter().find(|l| hay.contains(&l.to_lowercase())).map(|l| (*l).to_string())
    };

    // FN1 + FN2 per ADD directive.
    let mut verdicts = Vec::new();
    let mut leaked = Vec::new();
    let mut task_specific = Vec::new();
    for d in &input.bilan.add_directives {
        let hit = contains_literal(d);
        if let Some(lit) = &hit {
            if !leaked.iter().any(|l| l == lit) {
                leaked.push(lit.clone());
            }
            task_specific.push(d.clone());
        }
        let scope = if hit.is_some() { DirectiveScope::TaskSpecific } else { DirectiveScope::General };
        verdicts.push(DirectiveVerdict { directive: d.clone(), scope });
    }

    // Consensus of the three on the prompt change.
    let approvals = [input.votes.agent, input.votes.orchestrator, input.votes.olympe]
        .iter()
        .filter(|v| v.approve_prompt)
        .count();
    let unanimous = approvals == 3;
    let clean = leaked.is_empty() && task_specific.is_empty();

    // Outcome DERIVED from the three run_ok votes (majority), never self-report.
    let run_oks = [input.votes.agent, input.votes.orchestrator, input.votes.olympe]
        .iter()
        .filter(|v| v.run_ok)
        .count();
    let outcome = if run_oks >= 2 { Outcome::Success } else { Outcome::Problem };

    let mut rationale = vec![format!(
        "prompt approvals {approvals}/3; run_ok {run_oks}/3; leaked_literals {}; task_specific {}",
        leaked.len(),
        task_specific.len()
    )];

    let (decision, resulting_prompt) = if unanimous && clean {
        rationale.push("consensus + anti-over-fit clean → improved prompt adopted".into());
        (EvalDecision::Improved, apply_directives(&input.base_prompt, &input.bilan))
    } else {
        let why = if unanimous {
            "anti-over-fit veto (leaked literals or task-specific directive)"
        } else {
            "dissent (not unanimous)"
        };
        rationale.push(format!("statu quo: {why} → original prompt kept"));
        (EvalDecision::StatuQuo, input.base_prompt.clone())
    };

    EvalResult {
        report: EvalReport { decision, outcome, directive_verdicts: verdicts, leaked_literals: leaked, task_specific_directives: task_specific, rationale },
        resulting_prompt,
    }
}

/// Apply the bilan's prompt directives to the base prompt: drop lines matching a
/// `−consigne`, then append each `+consigne` as a new line. A minimal textual model of
/// "the improved prompt" (the eval already vetted the additions).
fn apply_directives(base: &str, bilan: &Bilan) -> String {
    let mut lines: Vec<String> = base
        .lines()
        .filter(|l| !bilan.drop_directives.iter().any(|d| !d.trim().is_empty() && l.contains(d.trim())))
        .map(str::to_string)
        .collect();
    for add in &bilan.add_directives {
        if !add.trim().is_empty() {
            lines.push(add.clone());
        }
    }
    lines.join("\n")
}

/// Does a token look like a CONCRETE task value (not prose)? — a digit, a path/id
/// separator, an internal/repeated uppercase (camelCase / ALLCAPS like `RB1`,
/// `TabState`, `catalog.jsonl`, `#1990`). Plain lowercase prose words are NOT values.
///
/// ponytail: a heuristic proxy for "task literal", not a parser. It won't catch a
/// bare lowercase project noun; the upgrade path is a real NER / stop-word model.
fn looks_like_task_value(t: &str) -> bool {
    let alnum = t.chars().filter(|c| c.is_alphanumeric()).count();
    if alnum < 2 {
        return false;
    }
    let uppers = t.chars().filter(char::is_ascii_uppercase).count();
    let has_internal_upper = t.chars().skip(1).any(|c| c.is_ascii_uppercase());
    t.chars().any(|c| c.is_ascii_digit())
        || t.chars().any(|c| matches!(c, '-' | '_' | '/' | '#' | '@' | '.'))
        || has_internal_upper
        || uppers >= 2
}

/// Extract the PRECISE task's concrete literals from a card's `objective` +
/// `current_task_log` (FN2 E3b).
///
/// These are the values the SV2 ban must keep out of the prompt. The daemon derives
/// them itself so the anti-over-fit gate is ENFORCED, never a declared-clean set
/// trusted from the orchestrator. Splits on whitespace + sentence
/// punctuation only, so identifiers (`catalog.jsonl`, `sess-abc`) stay whole.
#[must_use]
pub fn task_literals_of(card: &CatalogCard) -> Vec<String> {
    let mut lits: Vec<String> = Vec::new();
    let mut harvest = |text: &str| {
        for tok in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '"' | '\'' | '!' | '?')) {
            let t = tok.trim().trim_end_matches('.');
            if looks_like_task_value(t) && !lits.iter().any(|e| e == t) {
                lits.push(t.to_string());
            }
        }
    };
    if let Some(o) = &card.objective {
        harvest(o);
    }
    for l in &card.current_task_log {
        harvest(l);
    }
    lits
}

/// A retired agent's CARD — a verbatim COPY of the durable `build_snapshot` fields
/// (no new schema): the exact set that survives a restart on `TabState`.
///
/// `id` (the tab uuid) is the record id; `retired_at` stamps the archive. Every
/// field mirrors the card on `TabState`/`SnapshotTab`, so the round-trip is
/// byte-complete (RB1 acceptance 1).
///
/// **v2 (SV3)**: when `schema_version == Some(2)` the card also carries the distilled
/// skill profile (`skill` name = fold key, `prompt`, `tools`, `patterns`, …) and this
/// instance's per-mode telemetry (`spawn_mode`, `outcome`, `tokens`, `cost`). A v1
/// card has none of these (fields skipped) → byte-identical to before. The `baseline`
/// (invariant #2, A/B-isolated) is the existing top-level `session_id`/`agent_kind`,
/// surfaced separately in the read-model and EXCLUDED from the skill fold.
///
/// (No `Eq`: the v2 `cost` is an `f64`. `PartialEq` is all the round-trip + fold code
/// needs — the card is never a map key or set member.)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    ///
    /// SUPERSEDED by [`Self::bilan`] (SV1): when a structured bilan is supplied it
    /// back-fills this with a one-line digest, so legacy readers keep working while the
    /// bilan is the source of truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mission: Option<String>,
    /// The structured BILAN (SV1): the agent's retrospective ON ITS PROMPT, captured at
    /// retire BEFORE the éval (SV2). Replaces the 1-line `last_mission` as the closing
    /// record. `None` when the agent posted no bilan (v1 / legacy retire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bilan: Option<Bilan>,
    // ----- v2 (SV3) — all skipped when absent, so a v1 card is byte-identical -----
    /// The PROPER SKILL NAME (SV5-nom): a short, stable name = the v2 FOLD KEY,
    /// replacing the ugly whole-specialty slug. Carried by clones so N instances of a
    /// skill fold to one profile. `None` on a v1 card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Monotonic version of the distilled prompt for this skill (SV2 bumps it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<u32>,
    /// The DISTILLED prompt (generalised, precise context JETTISONED) — the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Profile: the tools this skill uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Profile: reusable patterns the skill applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// PER-INSTANCE: how this instance was born — the metric PARTITION key. `Origin`
    /// is excluded from the fresh-vs-resume A/B. `None` on a v1 card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_mode: Option<SpawnMode>,
    /// PER-INSTANCE: the retire outcome (from the éval-à-3, not self-report).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// PER-INSTANCE telemetry: tokens spent (reuses the agent-tokens signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// PER-INSTANCE telemetry: cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// OPTIONAL difficulty affordance (orchestrator) — anti-confound stratification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<u8>,
    /// Schema version. `Some(2)` = a v2 record (in the skill read-model);
    /// `None`/`Some(1)` = a v1 legacy record (QUARANTINED from the v2 read-model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// The éval-à-3 OUTPUT (SV2): the traced improved/statu-quo decision, the per-
    /// directive general|task-specific verdicts (FN1), the anti-over-fit findings
    /// (FN2), and the DERIVED outcome. `None` on a v1 / un-evaluated retire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalReport>,
    /// SC1 (#39) — the event-sourced record TYPE. `Retire` (default) is skipped from
    /// the JSON, so every existing record is byte-identical. `Edit`/`Delete`/`Restore`
    /// are the dashboard mutation records.
    #[serde(default, skip_serializing_if = "RecordKind::is_retire")]
    pub kind: RecordKind,
    /// Unix-millis the record was appended. For a retire it's the retire time; for an
    /// edit/delete/restore it's the mutation time (audit only — the fold authority is
    /// APPEND ORDER, not this timestamp).
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
            // v1 card: no v2 profile/telemetry (byte-identical to before).
            ..Default::default()
        }
    }

    /// Copy the durable card off a live [`crate::api::SnapshotTab`] — the LIVE
    /// retire path (RB-wire). Same field set as [`from_tab_state`], sourced from
    /// the daemon's in-memory snapshot instead of a persisted `TabState`.
    ///
    /// [`from_tab_state`]: CatalogCard::from_tab_state
    #[must_use]
    pub fn from_snapshot(t: &crate::api::SnapshotTab, after_action: Option<String>, retired_at: u64) -> Self {
        let s = |a: &Option<std::sync::Arc<str>>| a.as_deref().map(str::to_string);
        Self {
            id: t.id.to_string(),
            slug: card_slug(t.assignment.as_deref(), t.specialty.as_deref()),
            name: Some(t.name.to_string()).filter(|s| !s.is_empty()),
            assignment: s(&t.assignment),
            specialty: s(&t.specialty),
            orchestrator: s(&t.orchestrator),
            objective: s(&t.objective),
            current_task_log: t.current_task.clone(),
            conventions: t.conventions.clone(),
            evaluations: t.evaluations.clone(),
            usage_count: t.usage_count,
            last_used_at: t.last_used_at,
            session_id: s(&t.agent_session_id),
            agent_kind: s(&t.agent_kind),
            last_mission: after_action.filter(|s| !s.trim().is_empty()),
            // SV4: carry the tab's spawn_mode into the record (the A/B partition key,
            // SV5). A V2Stamp `spawn_mode` can still override it via `with_v2`.
            spawn_mode: t.spawn_mode,
            // SV5: the A/B token telemetry REUSES the tab's existing agent-tokens
            // counter (input+output) — zero new instrumentation. `with_v2` overrides
            // only if the orchestrator stamps a different figure.
            tokens: t.tokens.map(|u| u.input.saturating_add(u.output)),
            retired_at,
            // v1 card: no other v2 profile/telemetry (byte-identical to before).
            ..Default::default()
        }
    }

    /// The persist-gate predicate: is this RE-READ archive complete enough to close?
    ///
    /// A record must always carry a real `id`. Then the completeness bar depends on
    /// the schema:
    /// - **v1**: WHEN the tab carried a live session (`had_session`), the archive must
    ///   carry the `session_id` — a lost existing session is an incomplete archive.
    /// - **v2 (CF1, SV2)**: a profile is complete iff it has a NON-EMPTY `skill` AND a
    ///   NON-EMPTY `prompt`. `session_id` is NOT required — the v2 baseline is
    ///   A/B-isolated and optional. This is the WRITE gate: `perform_retire` refuses to
    ///   close a v2 tab whose profile lacks skill+prompt, so a half-built profile can
    ///   never die at close.
    #[must_use]
    pub fn is_complete(&self, had_session: bool) -> bool {
        if self.id.is_empty() {
            return false;
        }
        if self.is_v2() {
            return self.has_skill() && self.has_prompt();
        }
        !had_session || self.session_id.is_some()
    }

    /// A v2 record — folded into the skill read-model. `schema_version == Some(2)`.
    /// A v1/legacy card (`None`/`Some(1)`) is QUARANTINED from the v2 read-model.
    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.schema_version == Some(2)
    }

    /// CF1 (SV2): does this record carry a NON-EMPTY `skill`? The v2 completeness bar —
    /// a `schema_version:2` record without a skill is an incomplete profile (never
    /// closed, quarantined from the read-model).
    #[must_use]
    pub fn has_skill(&self) -> bool {
        self.skill.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    /// CF1 (SV2): does this record carry a NON-EMPTY `prompt`? The other half of the v2
    /// completeness bar — a profile with no distilled prompt can't be re-seeded.
    #[must_use]
    pub fn has_prompt(&self) -> bool {
        self.prompt.as_deref().is_some_and(|p| !p.trim().is_empty())
    }

    /// The v2 FOLD KEY (SV5-nom): the proper `skill` name, or — as a stable fallback
    /// until the agent's bilan names it (SV1/SV2) — the canonical `slug`, so a v2
    /// record is always foldable. Never the freeform tab name nor the tab-id.
    #[must_use]
    pub fn fold_key(&self) -> String {
        self.skill
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| self.slug.clone(), str::to_string)
    }

    /// Promote this v1 card to a v2 record by stamping the orchestrator's [`V2Stamp`]
    /// (SV3): the distilled profile plus this instance's per-mode telemetry. Sets
    /// `schema_version` to 2. The baseline (`session_id`/`agent_kind`) is untouched —
    /// it stays A/B-isolated.
    #[must_use]
    pub fn with_v2(mut self, stamp: V2Stamp) -> Self {
        self.skill = stamp.skill.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.prompt_version = stamp.prompt_version;
        self.prompt = stamp.prompt;
        self.tools = stamp.tools;
        self.patterns = stamp.patterns;
        // SV4: keep the spawn-time spawn_mode (from the tab) unless the stamp overrides.
        if stamp.spawn_mode.is_some() {
            self.spawn_mode = stamp.spawn_mode;
        }
        self.outcome = stamp.outcome;
        // SV5: keep the telemetry-sourced tokens (from_snapshot) unless the stamp gives
        // an explicit figure. Cost is stamp-only (no existing per-tab cost telemetry).
        if stamp.tokens.is_some() {
            self.tokens = stamp.tokens;
        }
        if stamp.cost.is_some() {
            self.cost = stamp.cost;
        }
        self.difficulty = stamp.difficulty;
        self.schema_version = Some(2);
        self
    }

    /// Attach the agent's structured [`Bilan`] (SV1) — the retrospective on its prompt,
    /// captured at retire BEFORE the éval. It REPLACES the 1-line `last_mission` as the
    /// closing record: the structured bilan is the source of truth, and `last_mission`
    /// is back-filled with a one-line digest so legacy readers keep working. An empty
    /// bilan is a no-op (nothing to record).
    #[must_use]
    pub fn with_bilan(mut self, bilan: Bilan) -> Self {
        if !bilan.is_empty() {
            self.last_mission = Some(bilan.one_line());
            self.bilan = Some(bilan);
        }
        self
    }

    /// Apply an éval-à-3 [`EvalResult`] (SV2) to this v2 record: the DERIVED outcome
    /// overrides any self-reported one, the resulting (improved or statu-quo) prompt
    /// becomes the record's `prompt`, and the full eval report (FN1 verdicts + FN2
    /// findings + decision + trace) is stored for audit. The eval OWNS the outcome and
    /// the final prompt — that's what makes the outcome eval-derived, not self-report.
    #[must_use]
    pub fn with_eval(mut self, result: EvalResult) -> Self {
        self.outcome = Some(result.report.outcome);
        self.prompt = Some(result.resulting_prompt).filter(|p| !p.is_empty());
        self.eval = Some(result.report);
        self
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
// SV3 — the v2 SKILL read-model: fold retired records BY SKILL NAME into one
// mode-agnostic profile + per-mode metrics + a DERIVED fresh-vs-resume compare.
//
// The mode PARTITIONS the metrics, NOT the profile (1 skill = 1 skill however its
// instances were born). `fresh_vs_resume` is DERIVED at read, never stored (the S4
// read-only discipline). v1 records are QUARANTINED (filtered by `is_v2`).
// ---------------------------------------------------------------------------

/// Per-mode aggregate metrics for one arm of the A/B (`fresh` or `resume`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeMetrics {
    pub spawns: u64,
    pub success: u64,
    pub problem: u64,
    /// Mean tokens over the arm's instances that reported tokens (`None` = none did).
    pub tokens_avg: Option<f64>,
    /// Mean cost over the arm's instances that reported cost.
    pub cost_avg: Option<f64>,
}

impl ModeMetrics {
    /// Aggregate the instances of `mode`. `Origin` is never an arm (excluded from A/B).
    fn of(instances: &[CatalogCard], mode: SpawnMode) -> Self {
        let arm: Vec<&CatalogCard> = instances.iter().filter(|c| c.spawn_mode == Some(mode)).collect();
        let toks: Vec<u64> = arm.iter().filter_map(|c| c.tokens).collect();
        let costs: Vec<f64> = arm.iter().filter_map(|c| c.cost).collect();
        Self {
            spawns: arm.len() as u64,
            success: arm.iter().filter(|c| c.outcome == Some(Outcome::Success)).count() as u64,
            problem: arm.iter().filter(|c| c.outcome == Some(Outcome::Problem)).count() as u64,
            tokens_avg: (!toks.is_empty()).then(|| toks.iter().sum::<u64>() as f64 / toks.len() as f64),
            cost_avg: (!costs.is_empty()).then(|| costs.iter().sum::<f64>() / costs.len() as f64),
        }
    }

    /// Delivery = success / judged (success + problem). `None` when nothing was judged.
    fn success_rate(&self) -> Option<f64> {
        let judged = self.success + self.problem;
        (judged > 0).then(|| self.success as f64 / judged as f64)
    }
}

/// The two benched arms — the metric PARTITION. `origin` is excluded from both.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByMode {
    pub fresh: ModeMetrics,
    pub resume: ModeMetrics,
}

/// `metrics.byMode` — the partitioned metrics wrapper (schema path `metrics.byMode.*`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub by_mode: ByMode,
}

/// SV5 G1 — the minimum instances PER ARM before the A/B yields a directional verdict.
///
/// Below this, the verdict is [`AbVerdict::InsufficientSample`] (the raw deltas are
/// still surfaced, but never interpreted).
///
/// ponytail: a heuristic floor for a dogfood-scale ledger, tunable — not a power
/// analysis. The upgrade path is a proper significance test once N is large.
pub const MIN_SAMPLE: u64 = 3;

/// SV5 G1 — the dead-zone on the delivery delta: below this the arms are called
/// [`AbVerdict::Inconclusive`] rather than favouring either. ponytail: heuristic.
const DELIVERY_DEAD_ZONE: f64 = 0.15;

/// The DIRECTIONAL A/B verdict (SV5 G3) — never a per-task pass/fail, always a trend
/// surfaced WITH its sample size (`fresh_n`/`resume_n`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AbVerdict {
    /// G1: an arm has fewer than [`MIN_SAMPLE`] instances → conclude NOTHING.
    #[default]
    InsufficientSample,
    /// Enough data, but the delivery delta is within the dead-zone → no clear winner.
    Inconclusive,
    /// Fresh delivers better on this skill (directional).
    FreshFavored,
    /// Resume delivers better on this skill (directional).
    ResumeFavored,
}

/// The fresh-vs-resume comparison — DERIVED at read, never stored (S4 discipline).
///
/// The raw `delivery_delta` / `tokens_ratio` / `cost_ratio` are `None` when a side
/// lacks the data. SV5 adds a GUARDED, directional [`verdict`](Self::verdict) surfaced
/// WITH its per-arm sample size (G3) — gated by [`MIN_SAMPLE`] (G1) and always computed
/// WITHIN one skill's own instances (G2, structural: the read-model folds by skill).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreshVsResume {
    /// `fresh.success_rate − resume.success_rate` (positive ⇒ fresh delivers better).
    pub delivery_delta: Option<f64>,
    /// `fresh.tokensAvg / resume.tokensAvg`.
    pub tokens_ratio: Option<f64>,
    /// `fresh.costAvg / resume.costAvg`.
    pub cost_ratio: Option<f64>,
    /// SV5 G3 — the directional verdict (never pass/fail), guarded by G1.
    pub verdict: AbVerdict,
    /// SV5 G3 — the sample size the verdict rests on, always surfaced.
    pub fresh_n: u64,
    pub resume_n: u64,
}

impl FreshVsResume {
    fn derive(fresh: &ModeMetrics, resume: &ModeMetrics) -> Self {
        let ratio = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) if b != 0.0 => Some(a / b),
            _ => None,
        };
        let delivery_delta = match (fresh.success_rate(), resume.success_rate()) {
            (Some(f), Some(r)) => Some(f - r),
            _ => None,
        };
        // G1 (min-sample) → G3 (directional, surfaced with n). G2 is structural: `fresh`
        // and `resume` are this ONE skill's arms (the caller folded by skill).
        let verdict = if fresh.spawns < MIN_SAMPLE || resume.spawns < MIN_SAMPLE {
            AbVerdict::InsufficientSample
        } else {
            match delivery_delta {
                Some(d) if d > DELIVERY_DEAD_ZONE => AbVerdict::FreshFavored,
                Some(d) if d < -DELIVERY_DEAD_ZONE => AbVerdict::ResumeFavored,
                _ => AbVerdict::Inconclusive,
            }
        };
        Self {
            delivery_delta,
            tokens_ratio: ratio(fresh.tokens_avg, resume.tokens_avg),
            cost_ratio: ratio(fresh.cost_avg, resume.cost_avg),
            verdict,
            fresh_n: fresh.spawns,
            resume_n: resume.spawns,
        }
    }
}

/// One folded SKILL in the v2 read-model: the mode-agnostic profile (latest-wins) +
/// partitioned metrics + the derived fresh-vs-resume compare.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillProfile {
    /// The proper skill NAME — the fold key (SV5-nom).
    pub skill: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specialty: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conventions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// usageCount summed across every retirement of this skill (all modes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_count: Option<u64>,
    /// The latest retirement's timestamp (the profile winner).
    pub retired_at: u64,
    pub metrics: Metrics,
    pub fresh_vs_resume: FreshVsResume,
    /// `SC1b` (#39) — `true` only for a TOMBSTONED skill surfaced via
    /// `?includeDeleted`. Skipped when `false`, so the normal read-model shape (which
    /// never lists deleted skills anyway) is byte-unchanged — the frozen contract holds.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

/// serde `skip_serializing_if` for a `bool` field that should vanish when `false`.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

impl SkillProfile {
    /// Fold one skill's records: the CONTENT axis (profile fields) = the LAST
    /// `{Retire|Edit}` record in APPEND ORDER (SC1 — not a timestamp compare: an `Edit`
    /// has no retire time and clocks skew; append order is the event-sourced authority,
    /// like `read_back`'s `.rev()`). Metrics aggregate the retire data-points (mutation
    /// records carry no `spawn_mode`, so they're inert for `byMode`).
    fn fold(skill: String, records: &[CatalogCard]) -> Self {
        // `records` is in append (file) order. The content winner is the LAST content
        // record. A visible group always has ≥1 content record; guard anyway.
        let Some(latest) = records.iter().rfind(|c| c.kind.is_content()) else {
            return Self { skill, ..Default::default() };
        };
        let usage_total: u64 = records.iter().filter_map(|c| c.usage_count).sum();
        let fresh = ModeMetrics::of(records, SpawnMode::Fresh);
        let resume = ModeMetrics::of(records, SpawnMode::Resume);
        let fresh_vs_resume = FreshVsResume::derive(&fresh, &resume);
        Self {
            skill,
            prompt_version: latest.prompt_version,
            prompt: latest.prompt.clone(),
            specialty: latest.specialty.clone(),
            conventions: latest.conventions.clone(),
            tools: latest.tools.clone(),
            patterns: latest.patterns.clone(),
            usage_count: (usage_total > 0).then_some(usage_total),
            retired_at: latest.retired_at,
            metrics: Metrics { by_mode: ByMode { fresh, resume } },
            fresh_vs_resume,
            // Visible by default; `read_skill_profiles_all_at` flips this for tombstones.
            deleted: false,
        }
    }
}

/// The v2 SKILL read-model over a catalogue file.
///
/// v2 records only (v1 quarantined), folded by skill name, sorted by skill for a
/// stable presentation. Path-injectable; a missing file reads empty. READ-ONLY —
/// nothing is written or compacted.
///
/// CF1 (SV2): only COMPLETE v2 profiles fold in — a `schema:2` record without a
/// non-empty skill (an incomplete write that a `RETIRE INCOMPLET` kept on disk) is
/// quarantined too, so the read-model never shows a half-built profile.
///
/// SC1 (#39) — the VISIBILITY axis: a skill whose LAST `{Delete|Restore}` record is a
/// `Delete` is TOMBSTONED and filtered out (derived at read, never materialised). An
/// `Edit` or a `Retire` never flips visibility — resurrection is a `Restore` only.
#[must_use]
pub fn read_skill_profiles_at(path: &Path) -> Vec<SkillProfile> {
    use std::collections::BTreeMap;
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let mut by_skill: BTreeMap<String, Vec<CatalogCard>> = BTreeMap::new();
    for c in parse_catalog(&body).into_iter().filter(|c| c.is_v2() && c.has_skill()) {
        by_skill.entry(c.fold_key()).or_default().push(c);
    }
    by_skill
        .into_iter()
        .filter(|(_, group)| is_visible(group))
        .map(|(skill, group)| SkillProfile::fold(skill, &group))
        .collect()
}

/// SC1 VISIBILITY axis: a skill is visible unless its LAST visibility record
/// (`Delete`/`Restore`, in append order) is a `Delete`. No visibility record ⇒ visible
/// (the normal case). `records` is in append order.
fn is_visible(records: &[CatalogCard]) -> bool {
    records
        .iter()
        .rev()
        .find(|c| c.kind.is_visibility())
        .is_none_or(|c| c.kind != RecordKind::Delete)
}

/// [`read_skill_profiles_at`] against the live [`catalog_path`] — the v2 `skills`
/// read-model of `GET /catalog/list` + `/dashboard/state`. READ-ONLY.
#[must_use]
pub fn read_skill_profiles() -> Vec<SkillProfile> {
    read_skill_profiles_at(&catalog_path())
}

/// `SC1b` (#39) — the read-model INCLUDING tombstoned skills, each marked `deleted:true`.
///
/// Same fold as [`read_skill_profiles_at`] but WITHOUT the visibility filter: a
/// tombstoned skill folds too, with `deleted = true`, so the dashboard can reach the
/// Restore action (`?includeDeleted`). Visible skills are byte-identical (`deleted`
/// skipped). Path-injectable; READ-ONLY.
#[must_use]
pub fn read_skill_profiles_all_at(path: &Path) -> Vec<SkillProfile> {
    use std::collections::BTreeMap;
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let mut by_skill: BTreeMap<String, Vec<CatalogCard>> = BTreeMap::new();
    for c in parse_catalog(&body).into_iter().filter(|c| c.is_v2() && c.has_skill()) {
        by_skill.entry(c.fold_key()).or_default().push(c);
    }
    by_skill
        .into_iter()
        .map(|(skill, group)| {
            let deleted = !is_visible(&group);
            SkillProfile { deleted, ..SkillProfile::fold(skill, &group) }
        })
        .collect()
}

/// [`read_skill_profiles_all_at`] against the live [`catalog_path`] — the
/// `GET /catalog/list?includeDeleted` read-model. READ-ONLY.
#[must_use]
pub fn read_skill_profiles_all() -> Vec<SkillProfile> {
    read_skill_profiles_all_at(&catalog_path())
}

// ---------------------------------------------------------------------------
// SC1 (#39) — catalogue MUTATIONS (dashboard): edit / delete / restore. Each is an
// APPEND (event-sourced): the fold above derives the new state. The route wraps these
// PURE builders in a read-modify-append ATOMIC under the daemon lock + a read-back gate.
//
// ponytail (borne 1b): the file grows unbounded (one record per edit). The ceiling is
// a COMPACTION pass — fold to latest-content-per-skill while KEEPING the last
// visibility record (tombstone/restore) + the retire metric data-points — run at a low
// cadence. Non-blocking (edits are rare, human-driven); the audit↔bounded-file tension
// is deferred, not solved here.
// ---------------------------------------------------------------------------

/// The body of `POST /catalog/{skill}/edit`. Absent fields are carried from the latest
/// content record; `prompt_version` is an OPTIONAL optimistic-concurrency token.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBody {
    pub specialty: Option<String>,
    pub prompt: Option<String>,
    pub conventions: Option<Vec<String>>,
    /// The `promptVersion` the editor SAW. If it no longer matches the latest, a
    /// concurrent edit landed first → [`EditError::Conflict`] (no lost update).
    pub prompt_version: Option<u32>,
}

/// Why an edit was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    /// No existing content record for the skill — nothing to edit.
    NotFound,
    /// The edit would leave skill or prompt empty (CF1 extended to edits — borne 4).
    EmptyProfile,
    /// The editor's `promptVersion` is stale — a concurrent edit already bumped it.
    Conflict,
}

/// The LATEST content record (`Retire|Edit`) for `skill` in APPEND ORDER, or `None`.
#[must_use]
pub fn latest_content_for<'a>(records: &'a [CatalogCard], skill: &str) -> Option<&'a CatalogCard> {
    records.iter().rfind(|c| c.has_skill() && c.fold_key() == skill && c.kind.is_content())
}

/// Does the catalogue hold ANY record for `skill`? (a `delete`/`restore` of a skill
/// that never existed is a no-op.)
#[must_use]
pub fn skill_exists(records: &[CatalogCard], skill: &str) -> bool {
    records.iter().any(|c| c.has_skill() && c.fold_key() == skill)
}

/// Plan an EDIT (SC1) — the PURE read-modify half.
///
/// Given the latest content record + the edit body, build the new `Edit` record
/// (`promptVersion = latest+1`, absent fields carried from latest, tools/patterns
/// carried) or reject. The append happens
/// under the lock (see the route).
///
/// # Errors
/// [`EditError::NotFound`] (no such skill), [`EditError::Conflict`] (stale version),
/// [`EditError::EmptyProfile`] (CF1: skill/prompt would be empty).
pub fn plan_edit(latest: Option<&CatalogCard>, skill: &str, body: &EditBody, now: u64) -> Result<CatalogCard, EditError> {
    let latest = latest.ok_or(EditError::NotFound)?;
    // Optimistic concurrency (borne 3): a stale expected version → refuse.
    if body.prompt_version.is_some_and(|expected| latest.prompt_version.unwrap_or(0) != expected) {
        return Err(EditError::Conflict);
    }
    // Absent fields carry from the latest content; an explicit empty is kept (and
    // caught by CF1 below).
    let prompt = body.prompt.clone().or_else(|| latest.prompt.clone());
    let specialty = body.specialty.clone().or_else(|| latest.specialty.clone());
    let conventions = body.conventions.clone().unwrap_or_else(|| latest.conventions.clone());
    // CF1 extended (borne 4): the result must keep skill + prompt non-empty.
    if skill.trim().is_empty() || prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
        return Err(EditError::EmptyProfile);
    }
    Ok(CatalogCard {
        skill: Some(skill.to_string()),
        prompt,
        specialty,
        conventions,
        tools: latest.tools.clone(),
        patterns: latest.patterns.clone(),
        prompt_version: Some(latest.prompt_version.unwrap_or(0).saturating_add(1)),
        kind: RecordKind::Edit,
        schema_version: Some(2),
        retired_at: now,
        ..Default::default()
    })
}

/// Build a VISIBILITY record (`Delete`/`Restore`) for `skill` — no content, just the
/// tombstone flip. The fold's visibility axis (last-wins) reads it.
#[must_use]
pub fn visibility_record(skill: &str, kind: RecordKind, now: u64) -> CatalogCard {
    CatalogCard {
        skill: Some(skill.to_string()),
        kind,
        schema_version: Some(2),
        retired_at: now,
        ..Default::default()
    }
}

/// The RAW catalogue records (no fold) — used to look up an A/B baseline session for
/// a skill, which the folded read-model deliberately drops. A missing file reads empty.
#[must_use]
pub fn read_catalog_cards() -> Vec<CatalogCard> {
    read_catalog_cards_at(&catalog_path())
}

/// [`read_catalog_cards`] against an explicit path — the SC1 mutation routes read +
/// re-read (read-back gate) the same file under the lock. A missing file reads empty.
#[must_use]
pub fn read_catalog_cards_at(path: &Path) -> Vec<CatalogCard> {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    parse_catalog(&body)
}

/// The LAST visibility record's kind (`Delete`/`Restore`) for `skill` in append order —
/// the SC1 read-back gate confirms a delete/restore landed. `None` if none exists.
#[must_use]
pub fn last_visibility_for(records: &[CatalogCard], skill: &str) -> Option<RecordKind> {
    records
        .iter()
        .rev()
        .find(|c| c.has_skill() && c.fold_key() == skill && c.kind.is_visibility())
        .map(|c| c.kind)
}

// ---------------------------------------------------------------------------
// SV4 — spawn --from-skill <name>: CREATE a real tab seeded from a skill's folded
// profile. Default = fresh+adapt (profile prompt + task overlay); `--resume` = the
// A/B baseline bench (reuse `restore_resume_command` on the baseline session). Both
// stamp the correct `spawn_mode` on the new tab for the SV5 metrics. Matching is by
// the proper skill NAME (SV5-nom), never a short-id/slug (the 🟡(ii) fix).
// ---------------------------------------------------------------------------

/// The fresh-spawn launcher: `claude` already in auto permission mode (mirrors
/// spawn-bot.sh, so a fresh agent doesn't stall on per-tool approvals).
pub const FRESH_LAUNCHER: &str = "claude --permission-mode auto";

/// Resolve a folded skill profile by its proper NAME (SV5-nom).
///
/// Exact first, then case-insensitive. `None` when no skill matches. NEVER a short-id /
/// slug — that was the 🟡(ii) bug; a spawn matches the human-given skill name.
#[must_use]
pub fn resolve_skill_profile<'a>(profiles: &'a [SkillProfile], name: &str) -> Option<&'a SkillProfile> {
    profiles.iter().find(|p| p.skill == name).or_else(|| {
        let n = name.to_lowercase();
        profiles.iter().find(|p| p.skill.to_lowercase() == n)
    })
}

/// The A/B BASELINE (`sessionId`, `agentKind`) for a skill.
///
/// The LATEST retired instance of that skill that carries a session. The read-model
/// DROPS the baseline (A/B-isolated), so `--resume` looks it up on the instance records
/// here. A missing `agent_kind` defaults to `"claude"`. `None` when there's no session.
#[must_use]
pub fn resolve_skill_baseline(cards: &[CatalogCard], skill: &str) -> Option<(String, String)> {
    cards
        .iter()
        .filter(|c| c.is_v2() && c.has_skill() && c.fold_key() == skill && c.session_id.is_some())
        .max_by_key(|c| c.retired_at)
        .map(|c| {
            (c.session_id.clone().unwrap_or_default(), c.agent_kind.clone().unwrap_or_else(|| "claude".to_string()))
        })
}

/// How a `spawn --from-skill` launches + what card to seed on the new tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSpawnPlan {
    pub skill: String,
    /// `Fresh` (default) or `Resume` — stamped on the created tab for the A/B (SV5).
    pub spawn_mode: SpawnMode,
    /// The launcher command to run in the new tab's shell.
    pub cmd: String,
    /// The prompt to send: fresh = distilled profile prompt + task overlay; resume =
    /// the task overlay only (the resumed session already carries its context).
    pub prompt: String,
    pub specialty: Option<String>,
    pub conventions: Vec<String>,
}

/// Build the spawn plan (PURE).
///
/// DEFAULT = fresh+adapt: the profile prompt + a `--task` overlay, `SpawnMode::Fresh`,
/// the [`FRESH_LAUNCHER`]. `resume` = the baseline bench: reuse
/// [`crate::restore_resume_command`] on the baseline session, `SpawnMode::Resume` —
/// erroring if the skill has no baseline session to resume.
///
/// # Errors
/// `--resume` with no baseline, or a kind with no resume command.
pub fn plan_from_skill(
    profile: &SkillProfile,
    baseline: Option<(&str, &str)>,
    task: Option<&str>,
    resume: bool,
) -> Result<SkillSpawnPlan, String> {
    let overlay = |base: &str| match task.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if base.is_empty() => format!("Task: {t}"),
        Some(t) => format!("{base}\n\nTask: {t}"),
        None => base.to_string(),
    };
    let (spawn_mode, cmd, prompt) = if resume {
        let (sid, kind) = baseline.ok_or_else(|| format!("skill '{}' has no baseline session to --resume", profile.skill))?;
        let cmd = crate::restore_resume_command(Some(kind), Some(sid), None)
            .ok_or_else(|| format!("cannot build a resume command for agent kind '{kind}'"))?;
        (SpawnMode::Resume, cmd, overlay(""))
    } else {
        (SpawnMode::Fresh, FRESH_LAUNCHER.to_string(), overlay(profile.prompt.as_deref().unwrap_or_default()))
    };
    Ok(SkillSpawnPlan {
        skill: profile.skill.clone(),
        spawn_mode,
        cmd,
        prompt,
        specialty: profile.specialty.clone(),
        conventions: profile.conventions.clone(),
    })
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
    // SV4: the new create-a-real-tab path, matched by proper skill NAME.
    if let Some(name) = arg_after(args, "--from-skill") {
        return spawn_from_skill_run(name, arg_after(args, "--task"), args.iter().any(|a| a == "--resume"));
    }
    let Some(key) = arg_after(args, "--from-card") else {
        eprintln!("usage:\n  tab-atelier spawn --from-skill <name> [--task <ctx>] [--resume]\n  tab-atelier spawn --from-card <id|slug> [--resume]");
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

/// `tab-atelier spawn --from-skill <name> [--task <ctx>] [--resume]` (SV4).
///
/// Resolve the folded profile by its proper NAME, build the plan (default fresh+adapt,
/// `--resume` = baseline bench), CREATE A REAL TAB (reusing the `dispatch --new` path,
/// the 🟡(i) fix — not plan-only), and seed its card (`spawn-mode` + specialty +
/// conventions) so the eventual retire records the right A/B `spawn_mode` (SV5).
fn spawn_from_skill_run(name: &str, task: Option<&str>, resume: bool) -> i32 {
    let profiles = read_skill_profiles();
    let Some(profile) = resolve_skill_profile(&profiles, name) else {
        eprintln!("spawn: no skill named '{name}' (matching is by proper name, not id/slug)");
        return 1;
    };
    let baseline_owned = resume.then(|| resolve_skill_baseline(&read_catalog_cards(), &profile.skill)).flatten();
    let baseline = baseline_owned.as_ref().map(|(s, k)| (s.as_str(), k.as_str()));
    let plan = match plan_from_skill(profile, baseline, task, resume) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("spawn: {e}");
            return 1;
        }
    };

    let uuid = match crate::cli::delegate::spawn_tab(Some(&plan.skill), None, &plan.cmd, &plan.prompt) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("spawn: {e}");
            return 1;
        }
    };

    // Seed the new tab's card so its retire records the A/B partition + the profile.
    let mode = match plan.spawn_mode {
        SpawnMode::Fresh => "fresh",
        SpawnMode::Resume => "resume",
        SpawnMode::Origin => "origin",
    };
    seed_card_verb(&uuid, "spawn-mode", "spawn_mode", mode);
    if let Some(sp) = &plan.specialty {
        seed_card_verb(&uuid, "specialty", "specialty", sp);
    }
    if !plan.conventions.is_empty() {
        seed_card_verb(&uuid, "conventions", "conventions", &plan.conventions.join(","));
    }
    println!("{}", serde_json::json!({ "spawned": uuid, "skill": plan.skill, "spawnMode": mode }));
    0
}

/// Best-effort POST of one card verb on the freshly-spawned tab (`spawn-mode`,
/// `specialty`, `conventions`). A seed failure is logged, never fatal — the tab exists.
fn seed_card_verb(uuid: &str, verb: &str, key: &str, value: &str) {
    let Ok(ep) = crate::cli::share_link::discover_endpoint() else {
        return;
    };
    let body = serde_json::json!({ key: value }).to_string();
    let r = crate::cli::share_link::agent()
        .post(format!("{}/tabs/by-id/{uuid}/{verb}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes());
    if r.is_err() {
        eprintln!("spawn: seeding {verb} on {uuid} failed (best-effort)");
    }
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
            ..Default::default()
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
            ..Default::default()
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

    // ----- SV3 + SV5-nom: the v2 skill schema + read-model ----------------------

    /// A v2 record: a named skill + per-instance mode/outcome/telemetry. `..Default`
    /// leaves the profile/baseline fields at their v1 defaults so each test sets only
    /// what it exercises.
    fn v2_card(
        id: &str,
        skill: &str,
        mode: SpawnMode,
        outcome: Option<Outcome>,
        retired: u64,
        tokens: Option<u64>,
        cost: Option<f64>,
    ) -> CatalogCard {
        CatalogCard {
            id: id.into(),
            skill: Some(skill.into()),
            spawn_mode: Some(mode),
            outcome,
            tokens,
            cost,
            schema_version: Some(2),
            retired_at: retired,
            ..Default::default()
        }
    }

    // SV3 acceptance (1): a v2 record round-trips byte-complete through the REAL
    // append + read-back — every profile + telemetry field survives.
    #[test]
    fn sv3_v2_record_round_trips_byte_complete() {
        let cat = TmpCatalog::new();
        let stamp = V2Stamp {
            skill: Some("rustsmith".into()),
            prompt_version: Some(3),
            prompt: Some("distilled prompt, no literals".into()),
            tools: vec!["cargo".into(), "grep".into()],
            patterns: vec!["read-back gate".into()],
            spawn_mode: Some(SpawnMode::Fresh),
            outcome: Some(Outcome::Success),
            tokens: Some(12_000),
            cost: Some(0.42),
            difficulty: Some(3),
        };
        let card = CatalogCard::from_tab_state(&maximal_tab_state("tab-v2"), None, RETIRED_AT).with_v2(stamp);
        assert!(card.is_v2(), "stamped card is v2");
        append_catalog_line(cat.path(), &card).expect("archive");
        let back = read_back(cat.path(), "tab-v2").expect("read-back");
        assert_eq!(back, card, "every v2 profile+telemetry field round-trips byte-complete");
        assert_eq!(back.skill.as_deref(), Some("rustsmith"));
        assert_eq!(back.prompt_version, Some(3));
        assert_eq!(back.tools, vec!["cargo", "grep"]);
        assert_eq!(back.patterns, vec!["read-back gate"]);
        assert_eq!(back.spawn_mode, Some(SpawnMode::Fresh));
        assert_eq!(back.outcome, Some(Outcome::Success));
        assert_eq!(back.tokens, Some(12_000));
        assert_eq!(back.cost, Some(0.42));
        assert_eq!(back.difficulty, Some(3));
        assert_eq!(back.schema_version, Some(2));
        // Baseline stays on the card (invariant #2), A/B-isolated — NOT nested in skill.
        assert_eq!(back.session_id.as_deref(), Some("sess-abc"), "baseline session archived");
    }

    // SV3: a v1 retire stays byte-identical — none of the v2 keys are emitted, so old
    // records are untouched and are the QUARANTINE marker (no schemaVersion:2).
    #[test]
    fn sv3_v1_record_carries_no_v2_fields() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        assert!(!card.is_v2(), "a plain retire is v1");
        let json = encode_catalog_line(&card);
        // ("tokens" is skipped too, but the nested Evaluation carries its own
        // token counts, so it's not a clean top-level discriminator — the keys
        // below are unambiguous v2 markers absent from any v1 record.)
        for absent in ["schemaVersion", "\"skill\"", "spawnMode", "\"prompt\"", "\"cost\"", "\"patterns\""] {
            assert!(!json.contains(absent), "v1 record must not carry the v2 key {absent}: {json}");
        }
    }

    // SV3 acceptance (2)+(5): the read-model folds by skill MODE-AGNOSTICALLY
    // (latest-wins profile), aggregates usageCount, and QUARANTINES v1 records.
    #[test]
    fn sv3_read_model_folds_by_skill_mode_agnostic_and_quarantines_v1() {
        let cat = TmpCatalog::new();
        let mut a = v2_card("i1", "rustsmith", SpawnMode::Fresh, Some(Outcome::Success), 100, Some(1000), None);
        a.usage_count = Some(2);
        a.prompt = Some("older prompt".into());
        a.prompt_version = Some(1);
        let mut b = v2_card("i2", "rustsmith", SpawnMode::Resume, Some(Outcome::Problem), 200, Some(2000), None);
        b.usage_count = Some(3);
        b.prompt = Some("newer prompt".into());
        b.prompt_version = Some(2);
        b.specialty = Some("rust daemon".into());
        // A v1 legacy record → QUARANTINED from the v2 read-model.
        let v1 = CatalogCard::from_tab_state(&maximal_tab_state("legacy"), None, 300);
        append_catalog_line(cat.path(), &a).unwrap();
        append_catalog_line(cat.path(), &b).unwrap();
        append_catalog_line(cat.path(), &v1).unwrap();

        let profiles = read_skill_profiles_at(cat.path());
        assert_eq!(profiles.len(), 1, "v1 quarantined → only the one v2 skill folds");
        let p = &profiles[0];
        assert_eq!(p.skill, "rustsmith");
        // Profile = the LATEST retirement (mode-agnostic): b (retired 200) wins over a.
        assert_eq!(p.prompt.as_deref(), Some("newer prompt"), "latest-wins profile");
        assert_eq!(p.prompt_version, Some(2));
        assert_eq!(p.specialty.as_deref(), Some("rust daemon"));
        assert_eq!(p.usage_count, Some(5), "usageCount aggregated across modes (2+3)");
    }

    // SV3 acceptance (3): metrics are PARTITIONED by mode, and `origin` is EXCLUDED
    // from the fresh-vs-resume A/B.
    #[test]
    fn sv3_metrics_partitioned_by_mode_origin_excluded() {
        let cat = TmpCatalog::new();
        let cards = [
            v2_card("f1", "s", SpawnMode::Fresh, Some(Outcome::Success), 1, Some(100), Some(1.0)),
            v2_card("f2", "s", SpawnMode::Fresh, Some(Outcome::Success), 2, Some(200), Some(2.0)),
            v2_card("f3", "s", SpawnMode::Fresh, Some(Outcome::Problem), 3, Some(300), Some(3.0)),
            v2_card("r1", "s", SpawnMode::Resume, Some(Outcome::Success), 4, Some(1000), Some(9.0)),
            v2_card("o1", "s", SpawnMode::Origin, Some(Outcome::Success), 5, Some(9999), Some(99.0)),
        ];
        for c in &cards {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let p = &read_skill_profiles_at(cat.path())[0];
        let f = &p.metrics.by_mode.fresh;
        assert_eq!((f.spawns, f.success, f.problem), (3, 2, 1), "fresh arm counts");
        assert_eq!(f.tokens_avg, Some(200.0), "fresh tokensAvg (100+200+300)/3");
        let r = &p.metrics.by_mode.resume;
        assert_eq!((r.spawns, r.success, r.problem), (1, 1, 0), "resume arm counts");
        assert_eq!(r.tokens_avg, Some(1000.0));
        // The origin instance (9999 tokens) never leaked into either A/B arm.
        assert_ne!(f.tokens_avg, Some(9999.0), "origin excluded from fresh");
        assert_ne!(r.tokens_avg, Some(9999.0), "origin excluded from resume");
    }

    // SV3 acceptance (4): fresh_vs_resume is DERIVED at read and NOT STORED.
    #[test]
    fn sv3_fresh_vs_resume_is_derived_not_stored() {
        let cat = TmpCatalog::new();
        // fresh: 2 success / 1 problem → rate 2/3 ; tokensAvg 200 ; costAvg 2.0
        // resume: 1 success → rate 1.0 ; tokensAvg 1000 ; costAvg 10.0
        let cards = [
            v2_card("f1", "s", SpawnMode::Fresh, Some(Outcome::Success), 1, Some(100), Some(1.0)),
            v2_card("f2", "s", SpawnMode::Fresh, Some(Outcome::Success), 2, Some(200), Some(2.0)),
            v2_card("f3", "s", SpawnMode::Fresh, Some(Outcome::Problem), 3, Some(300), Some(3.0)),
            v2_card("r1", "s", SpawnMode::Resume, Some(Outcome::Success), 4, Some(1000), Some(10.0)),
        ];
        for c in &cards {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let p = &read_skill_profiles_at(cat.path())[0];
        let fvr = &p.fresh_vs_resume;
        let dd = fvr.delivery_delta.expect("delta derivable");
        assert!((dd - (2.0 / 3.0 - 1.0)).abs() < 1e-9, "delivery_delta = fresh_rate − resume_rate");
        assert!((fvr.tokens_ratio.expect("tokens ratio") - 0.2).abs() < 1e-9, "tokens_ratio = 200/1000");
        assert!((fvr.cost_ratio.expect("cost ratio") - 0.2).abs() < 1e-9, "cost_ratio = 2.0/10.0");
        // NOT STORED: no derived read-model field is persisted on disk.
        let body = std::fs::read_to_string(cat.path()).unwrap();
        for forbidden in ["freshVsResume", "deliveryDelta", "tokensRatio", "costRatio", "byMode"] {
            assert!(!body.contains(forbidden), "derived field must not be persisted: {forbidden}");
        }
    }

    // SV5-nom: the proper NAME (not the slug) is the stable fold key — N clones with
    // one name but different slugs/ids fold to ONE profile with aggregated metrics.
    #[test]
    fn sv5_proper_name_is_the_stable_fold_key_across_clones() {
        let cat = TmpCatalog::new();
        let mut c1 = v2_card("clone-1", "rustsmith", SpawnMode::Fresh, Some(Outcome::Success), 10, None, None);
        c1.slug = "builder-rust".into();
        c1.usage_count = Some(1);
        let mut c2 = v2_card("clone-2", "rustsmith", SpawnMode::Resume, Some(Outcome::Success), 20, None, None);
        c2.slug = "reviewer-sql".into(); // a DIFFERENT slug — proves name, not slug, folds
        c2.usage_count = Some(1);
        let mut c3 = v2_card("clone-3", "rustsmith", SpawnMode::Fresh, Some(Outcome::Problem), 30, None, None);
        c3.slug = "totally-different".into();
        c3.usage_count = Some(1);
        for c in [&c1, &c2, &c3] {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let profiles = read_skill_profiles_at(cat.path());
        assert_eq!(profiles.len(), 1, "N clones + one proper name → ONE profile (name, not slug, folds)");
        let p = &profiles[0];
        assert_eq!(p.skill, "rustsmith", "the proper name is the fold key");
        assert_eq!(p.usage_count, Some(3), "clones' usageCount aggregate under the name");
        assert_eq!(p.metrics.by_mode.fresh.spawns, 2, "two fresh clones");
        assert_eq!(p.metrics.by_mode.resume.spawns, 1, "one resume clone");
        // fold_key falls back to the slug ONLY when no proper name (stable default).
        let mut nameless = v2_card("x", "", SpawnMode::Fresh, None, 1, None, None);
        nameless.skill = None;
        nameless.slug = "fallback-slug".into();
        assert_eq!(nameless.fold_key(), "fallback-slug", "no proper name → stable slug fallback");
    }

    // ----- SV1: the structured bilan (retrospective on the prompt) --------------

    fn sample_bilan() -> Bilan {
        Bilan {
            learned: vec!["read-back before close is the core gate".into()],
            problems: vec!["the prompt lacked a lease-refresh reminder".into()],
            add_directives: vec!["always beat the lease on long slices".into()],
            drop_directives: vec!["drop the stale 'commit each file' note".into()],
        }
    }

    // SV1 acceptance (1)+(4): the 4 bilan fields are captured, structured, and
    // round-trip byte-complete; the bilan REPLACES lastMission (structured source +
    // a one-line digest back-filled for legacy readers).
    #[test]
    fn sv1_bilan_four_fields_round_trip_and_replace_last_mission() {
        let cat = TmpCatalog::new();
        let bilan = sample_bilan();
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT).with_bilan(bilan.clone());
        assert_eq!(card.bilan.as_ref(), Some(&bilan), "the structured 4-field bilan lands on the card");
        // Replaces lastMission: last_mission is back-filled with a digest of the bilan.
        let lm = card.last_mission.as_deref().expect("last_mission back-filled from the bilan");
        assert!(
            lm.contains("learned:") && lm.contains("problems:") && lm.contains("+prompt:") && lm.contains("−prompt:"),
            "the digest covers all four bilan facets: {lm}"
        );
        // Round-trip byte-complete through the REAL append + read-back.
        append_catalog_line(cat.path(), &card).unwrap();
        let back = read_back(cat.path(), "t").expect("read-back");
        let b = back.bilan.expect("all 4 bilan fields round-trip");
        assert_eq!(b, bilan, "every bilan field survives byte-complete");
        assert_eq!((b.learned.len(), b.problems.len(), b.add_directives.len(), b.drop_directives.len()), (1, 1, 1, 1));
    }

    // SV1 acceptance (2): the bilan is PROMPT-scoped (generalisable) — a distinct
    // channel of directives-on-the-prompt, NOT the run's precise task context, which
    // stays in `objective`/`current_task_log`, untouched.
    #[test]
    fn sv1_bilan_is_prompt_scoped_separate_from_precise_task_context() {
        // maximal_tab_state carries PRECISE context: objective "ship RB1" + a task log.
        let bilan = Bilan {
            add_directives: vec!["state the persist-gate invariant up front".into()],
            drop_directives: vec!["remove the outdated slug guidance".into()],
            ..Default::default()
        };
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT).with_bilan(bilan);
        let b = card.bilan.as_ref().unwrap();
        let joined =
            [&b.learned[..], &b.problems[..], &b.add_directives[..], &b.drop_directives[..]].concat().join(" ");
        // The bilan carries prompt directives, NOT the precise task facts.
        assert!(!joined.contains("ship RB1"), "bilan does not carry the precise objective");
        assert!(!joined.contains("step one"), "bilan does not carry the precise task log");
        // The precise context lives in its own fields, untouched by the bilan.
        assert_eq!(card.objective.as_deref(), Some("ship RB1"), "precise objective untouched");
        assert_eq!(card.current_task_log, vec!["step one", "step two"], "precise task log untouched");
    }

    // SV1 acceptance (3): the bilan is archived AT retire, BEFORE the close (and thus
    // before the éval SV2, which runs on the catalogued bilan) — captured at
    // write_catalog time, before the shutdown seam.
    #[test]
    fn sv1_bilan_is_archived_before_close_and_before_eval() {
        let bilan = sample_bilan();
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT).with_bilan(bilan.clone());
        let seen_at_write = RefCell::new(None);
        let out = perform_retire(
            &card,
            true,
            true,
            |c| {
                *seen_at_write.borrow_mut() = c.bilan.clone();
                Ok(())
            },
            |_id| Some(card.clone()),
            || Ok(()),
            || {
                // At close, the archive already carried the structured bilan — written
                // at archive time, before any post-archive (éval) step.
                assert_eq!(*seen_at_write.borrow(), Some(bilan.clone()), "bilan archived BEFORE close/éval");
                Ok(())
            },
        );
        assert_eq!(out, RetireOutcome::Retired);
    }

    // SV1: an EMPTY bilan records nothing and never clobbers a legit legacy
    // after-action — the replacement is opt-in and backward-compatible.
    #[test]
    fn sv1_empty_bilan_is_a_noop_and_after_action_stands() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), Some("legacy 1-line".into()), RETIRED_AT)
            .with_bilan(Bilan::default());
        assert!(card.bilan.is_none(), "an empty bilan records nothing");
        assert_eq!(card.last_mission.as_deref(), Some("legacy 1-line"), "the legacy after-action still stands");
    }

    // ----- SV2: the a-priori éval-à-3 + CF1 -------------------------------------

    fn vote(approve: bool, run_ok: bool) -> EvalVote {
        EvalVote { approve_prompt: approve, run_ok }
    }

    // SV2 acceptance: consensus + anti-over-fit clean → the prompt is IMPROVED (the
    // +directive is applied), the decision is traced, and the outcome is DERIVED from
    // the three run_ok votes (majority), never self-reported.
    #[test]
    fn sv2_consensus_clean_improves_prompt_and_derives_outcome() {
        let input = EvalInput {
            base_prompt: "be a lazy senior".into(),
            bilan: Bilan { add_directives: vec!["always state the invariant up front".into()], ..Default::default() },
            task_literals: vec![], // clean
            votes: EvalVotes { agent: vote(true, true), orchestrator: vote(true, true), olympe: vote(true, false) },
        };
        let r = evaluate(&input);
        assert_eq!(r.report.decision, EvalDecision::Improved, "unanimous approve + clean → improved");
        assert!(r.resulting_prompt.contains("always state the invariant up front"), "the +directive is in the prompt");
        assert!(r.resulting_prompt.contains("be a lazy senior"), "the base prompt is kept");
        assert_eq!(r.report.outcome, Outcome::Success, "2/3 run_ok → success (DERIVED, not self-report)");
        assert!(!r.report.rationale.is_empty(), "the decision is traced");
        // FN1: the general directive is tagged General in the eval output.
        assert_eq!(r.report.directive_verdicts.len(), 1);
        assert_eq!(r.report.directive_verdicts[0].scope, DirectiveScope::General);
    }

    // SV2 acceptance: dissent (not unanimous) → STATU QUO (original prompt kept),
    // traced; outcome derived from run_ok minority → problem.
    #[test]
    fn sv2_dissent_keeps_statu_quo_traced() {
        let input = EvalInput {
            base_prompt: "base".into(),
            bilan: Bilan { add_directives: vec!["a general directive".into()], ..Default::default() },
            votes: EvalVotes { agent: vote(true, true), orchestrator: vote(false, false), olympe: vote(true, false) },
            ..Default::default()
        };
        let r = evaluate(&input);
        assert_eq!(r.report.decision, EvalDecision::StatuQuo, "not unanimous → statu quo");
        assert_eq!(r.resulting_prompt, "base", "the original prompt is kept");
        assert!(r.report.rationale.iter().any(|s| s.contains("dissent")), "dissent is traced");
        assert_eq!(r.report.outcome, Outcome::Problem, "1/3 run_ok → problem (derived)");
    }

    // SV2 acceptance (FN2 + FN1): the anti-over-fit VETO beats a rubber-stamp
    // consensus — a directive leaking a task literal → statu quo, the literal flagged
    // (E3b), the directive fails the test-nouvelle-tâche (E3c) and is tagged
    // TaskSpecific in the eval output (FN1).
    #[test]
    fn sv2_anti_over_fit_vetoes_leaked_literal_even_on_consensus() {
        let input = EvalInput {
            base_prompt: "base".into(),
            bilan: Bilan {
                add_directives: vec!["keep the persist-gate invariant".into(), "remember to ship RB1 first".into()],
                ..Default::default()
            },
            task_literals: vec!["RB1".into(), "catalog.jsonl".into()],
            votes: EvalVotes { agent: vote(true, true), orchestrator: vote(true, true), olympe: vote(true, true) },
        };
        let r = evaluate(&input);
        assert_eq!(r.report.decision, EvalDecision::StatuQuo, "leaked literal vetoes the improvement");
        assert_eq!(r.resulting_prompt, "base", "over-fit → the original prompt is kept");
        assert!(r.report.leaked_literals.iter().any(|l| l == "RB1"), "the leaked literal is flagged (E3b)");
        assert!(
            r.report.task_specific_directives.iter().any(|d| d.contains("RB1")),
            "the leaking directive fails the test-nouvelle-tâche (E3c)"
        );
        // FN1: per-directive verdicts live in the eval OUTPUT (not the bilan).
        let general = r.report.directive_verdicts.iter().find(|v| v.directive.contains("persist-gate")).unwrap();
        assert_eq!(general.scope, DirectiveScope::General, "the clean directive is general");
        let specific = r.report.directive_verdicts.iter().find(|v| v.directive.contains("RB1")).unwrap();
        assert_eq!(specific.scope, DirectiveScope::TaskSpecific, "the leaking directive is task-specific");
    }

    // SV2 (FN2 source): task literals are derived from the PRECISE context
    // (objective + task log) — concrete values, not prose words.
    #[test]
    fn sv2_task_literals_extracted_from_precise_context_not_prose() {
        let card = CatalogCard::from_tab_state(&maximal_tab_state("t"), None, RETIRED_AT);
        // maximal: objective "ship RB1", current_task ["step one", "step two"].
        let lits = task_literals_of(&card);
        assert!(lits.iter().any(|l| l == "RB1"), "RB1 (has a digit) is a task literal");
        assert!(!lits.iter().any(|l| l == "ship" || l == "step" || l == "one"), "prose words are not literals");
        // Identifier-shaped values are caught WHOLE (not split on . or camelCase).
        let mut card2 = card;
        card2.objective = Some("wire catalog.jsonl and TabState".into());
        card2.current_task_log = vec![];
        let lits2 = task_literals_of(&card2);
        assert!(lits2.contains(&"catalog.jsonl".to_string()), "path id caught whole");
        assert!(lits2.contains(&"TabState".to_string()), "camelCase id caught");
        assert!(!lits2.contains(&"wire".to_string()) && !lits2.contains(&"and".to_string()), "prose skipped");
    }

    // CF1 (SV2, Olympe's guard): a v2 profile is INCOMPLETE without a non-empty skill
    // AND prompt; session_id is OPTIONAL in v2. The WRITE gate (perform_retire) refuses
    // to close an incomplete v2 profile — it can never die at close.
    #[test]
    fn sv2_cf1_v2_needs_skill_and_prompt_session_optional_and_gates_the_close() {
        // schema:2 but NO skill → incomplete.
        let mut noskill = v2_card("t", "builder", SpawnMode::Fresh, None, RETIRED_AT, None, None);
        noskill.skill = None;
        noskill.prompt = Some("p".into());
        assert!(noskill.is_v2() && !noskill.has_skill());
        assert!(!noskill.is_complete(true), "CF1: v2 without a skill is incomplete");
        // schema:2 + skill but NO prompt → incomplete.
        let mut noprompt = v2_card("t", "builder", SpawnMode::Fresh, None, RETIRED_AT, None, None);
        noprompt.prompt = None;
        assert!(!noprompt.is_complete(false), "CF1: v2 without a prompt is incomplete");
        // schema:2 + skill + prompt, NO session, had_session=true → COMPLETE (session
        // is OPTIONAL in v2: the baseline is A/B-isolated, not a completeness bar).
        let mut full = v2_card("t", "builder", SpawnMode::Fresh, None, RETIRED_AT, None, None);
        full.prompt = Some("distilled".into());
        full.session_id = None;
        assert!(full.is_complete(true), "CF1: v2 with skill+prompt is complete even had_session + no session_id");
        // The WRITE gate refuses to close the incomplete v2 profile.
        let out = perform_retire(
            &noprompt,
            true,
            false,
            |_c| Ok(()),
            |_id| Some(noprompt.clone()),
            || Ok(()),
            || panic!("must not close an incomplete v2 profile (no prompt)"),
        );
        assert!(matches!(out, RetireOutcome::Incomplete(_)), "CF1: a v2 retire never closes without skill+prompt");
    }

    // SV2: the eval-DERIVED outcome OVERRIDES a self-reported one; the full eval report
    // (FN1 verdicts + FN2 findings + trace) is stored on the record.
    #[test]
    fn sv2_eval_derived_outcome_overrides_self_report() {
        // A v2 card self-reporting Success…
        let card = v2_card("t", "builder", SpawnMode::Fresh, Some(Outcome::Success), RETIRED_AT, None, None);
        assert_eq!(card.outcome, Some(Outcome::Success), "self-reported success");
        // …but the eval derives Problem (1/3 run_ok) → the record's outcome is DERIVED.
        let result = evaluate(&EvalInput {
            base_prompt: "p".into(),
            votes: EvalVotes { agent: vote(false, false), orchestrator: vote(false, true), olympe: vote(false, false) },
            ..Default::default()
        });
        let evaled = card.with_eval(result);
        assert_eq!(evaled.outcome, Some(Outcome::Problem), "the eval-derived outcome overrides the self-report");
        assert!(evaled.eval.is_some(), "the eval report is stored (FN1 verdicts + FN2 findings + trace)");
    }

    // ----- SV4: spawn --from-skill (the planner) --------------------------------

    fn profile(skill: &str, prompt: &str) -> SkillProfile {
        SkillProfile {
            skill: skill.into(),
            prompt: Some(prompt.into()),
            specialty: Some("rust daemon".into()),
            conventions: vec!["CONVENTIONS.md".into()],
            ..Default::default()
        }
    }

    // SV4 (🟡 ii fix): resolve by proper NAME (exact, then case-insensitive) — NEVER a
    // slug / short-id.
    #[test]
    fn sv4_resolve_skill_profile_matches_by_name_not_slug() {
        let profiles = vec![profile("Rustsmith", "p1"), profile("reviewer", "p2")];
        assert_eq!(resolve_skill_profile(&profiles, "Rustsmith").map(|p| &p.skill), Some(&"Rustsmith".to_string()));
        assert_eq!(
            resolve_skill_profile(&profiles, "rustsmith").map(|p| &p.skill),
            Some(&"Rustsmith".to_string()),
            "case-insensitive by name"
        );
        assert!(resolve_skill_profile(&profiles, "builder-rust-daemon").is_none(), "a slug never matches (🟡 ii)");
        assert!(resolve_skill_profile(&profiles, "nope").is_none());
    }

    // SV4 default = fresh+adapt: profile prompt + task overlay, SpawnMode::Fresh.
    #[test]
    fn sv4_plan_default_is_fresh_with_task_overlay() {
        let p = profile("rustsmith", "be a lazy senior");
        let plan = plan_from_skill(&p, None, Some("fix the flaky test"), false).unwrap();
        assert_eq!(plan.spawn_mode, SpawnMode::Fresh);
        assert_eq!(plan.cmd, FRESH_LAUNCHER, "fresh launcher");
        assert!(plan.prompt.contains("be a lazy senior"), "the profile prompt is the base");
        assert!(plan.prompt.contains("Task: fix the flaky test"), "the --task overlay is appended");
        assert_eq!(plan.specialty.as_deref(), Some("rust daemon"), "card seed: specialty");
        assert_eq!(plan.conventions, vec!["CONVENTIONS.md"], "card seed: conventions");
        // No task → just the profile prompt (no overlay).
        assert_eq!(plan_from_skill(&p, None, None, false).unwrap().prompt, "be a lazy senior");
    }

    // SV4 --resume = baseline bench: reuse restore_resume_command on baseline.sessionId.
    #[test]
    fn sv4_plan_resume_reuses_baseline_session() {
        let p = profile("rustsmith", "base");
        let plan = plan_from_skill(&p, Some(("sess-champion", "claude")), Some("bench task"), true).unwrap();
        assert_eq!(plan.spawn_mode, SpawnMode::Resume);
        assert!(plan.cmd.contains("sess-champion"), "the resume command carries baseline.sessionId: {}", plan.cmd);
        assert!(plan.cmd.contains("resume"), "it's a --resume command: {}", plan.cmd);
        // --resume with NO baseline → error (can't bench without a champion session).
        assert!(plan_from_skill(&p, None, None, true).is_err(), "--resume needs a baseline");
    }

    // SV4: the A/B baseline (sessionId + kind) comes from the LATEST instance of the
    // skill carrying a session; other skills' instances are ignored.
    #[test]
    fn sv4_resolve_skill_baseline_picks_latest_session_of_the_skill() {
        let mut c1 = v2_card("i1", "rustsmith", SpawnMode::Fresh, Some(Outcome::Success), 100, None, None);
        c1.session_id = Some("old-sess".into());
        c1.agent_kind = Some("claude".into());
        let mut c2 = v2_card("i2", "rustsmith", SpawnMode::Resume, Some(Outcome::Success), 200, None, None);
        c2.session_id = Some("new-sess".into());
        c2.agent_kind = Some("claude".into());
        let mut other = v2_card("i3", "reviewer", SpawnMode::Fresh, None, 300, None, None);
        other.session_id = Some("other-sess".into());
        let cards = vec![c1, c2, other];
        let (sid, kind) = resolve_skill_baseline(&cards, "rustsmith").expect("baseline");
        assert_eq!(sid, "new-sess", "the LATEST instance's session wins");
        assert_eq!(kind, "claude");
        assert!(resolve_skill_baseline(&cards, "no-such-skill").is_none(), "no session → no baseline");
    }

    // ----- SV5-métriques: the byMode ledger + Olympe guards G1/G2/G3 ------------

    /// Fill an arm with `n` outcomes for a skill (helper for the guard tests).
    fn arm(skill: &str, mode: SpawnMode, outcome: Outcome, n: u64, base_at: u64) -> Vec<CatalogCard> {
        (0..n)
            .map(|i| {
                v2_card(&format!("{skill}-{mode:?}-{i}"), skill, mode, Some(outcome), base_at + i, None, None)
            })
            .collect()
    }

    // SV5 G1: below MIN_SAMPLE in an arm → InsufficientSample (conclude NOTHING); the
    // raw delta is still surfaced, but never interpreted.
    #[test]
    fn sv5_g1_min_sample_gates_the_verdict() {
        let cat = TmpCatalog::new();
        // fresh: 3 success ; resume: only 1 → resume_n < MIN_SAMPLE.
        let mut cards = arm("s", SpawnMode::Fresh, Outcome::Success, MIN_SAMPLE, 1);
        cards.extend(arm("s", SpawnMode::Resume, Outcome::Problem, 1, 100));
        for c in &cards {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let fvr = &read_skill_profiles_at(cat.path())[0].fresh_vs_resume;
        assert_eq!(fvr.verdict, AbVerdict::InsufficientSample, "resume arm under MIN → G1 gates the verdict");
        // The raw delta is still computed + n surfaced (never hidden).
        assert!(fvr.delivery_delta.is_some(), "raw delta still surfaced under G1");
        assert_eq!((fvr.fresh_n, fvr.resume_n), (MIN_SAMPLE, 1), "sample sizes surfaced");
    }

    // SV5 G3: with n ≥ MIN in both arms the verdict is DIRECTIONAL (never a per-task
    // pass/fail) and always carries its sample size.
    #[test]
    fn sv5_g3_directional_verdict_surfaced_with_n() {
        let cat = TmpCatalog::new();
        // fresh all success (rate 1.0) vs resume all problem (rate 0.0) → FreshFavored.
        let mut cards = arm("s", SpawnMode::Fresh, Outcome::Success, MIN_SAMPLE, 1);
        cards.extend(arm("s", SpawnMode::Resume, Outcome::Problem, MIN_SAMPLE, 100));
        for c in &cards {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let fvr = &read_skill_profiles_at(cat.path())[0].fresh_vs_resume;
        assert_eq!(fvr.verdict, AbVerdict::FreshFavored, "fresh 1.0 vs resume 0.0 → fresh favored");
        assert_eq!((fvr.fresh_n, fvr.resume_n), (MIN_SAMPLE, MIN_SAMPLE), "n surfaced with the trend (G3)");

        // Equal delivery (both success) → Inconclusive, NOT a false winner.
        let cat2 = TmpCatalog::new();
        let mut eq = arm("s", SpawnMode::Fresh, Outcome::Success, MIN_SAMPLE, 1);
        eq.extend(arm("s", SpawnMode::Resume, Outcome::Success, MIN_SAMPLE, 100));
        for c in &eq {
            append_catalog_line(cat2.path(), c).unwrap();
        }
        assert_eq!(
            read_skill_profiles_at(cat2.path())[0].fresh_vs_resume.verdict,
            AbVerdict::Inconclusive,
            "equal delivery within the dead-zone → inconclusive"
        );
    }

    // SV5 G2: the verdict is WITHIN one skill — two skills with opposite A/B results
    // are judged independently on their OWN arms (never cross-skill mixing).
    #[test]
    fn sv5_g2_verdict_is_within_skill_not_cross_skill() {
        let cat = TmpCatalog::new();
        // skill-a: fresh good / resume bad → FreshFavored.
        let mut cards = arm("skill-a", SpawnMode::Fresh, Outcome::Success, MIN_SAMPLE, 1);
        cards.extend(arm("skill-a", SpawnMode::Resume, Outcome::Problem, MIN_SAMPLE, 20));
        // skill-b: the MIRROR — fresh bad / resume good → ResumeFavored.
        cards.extend(arm("skill-b", SpawnMode::Fresh, Outcome::Problem, MIN_SAMPLE, 40));
        cards.extend(arm("skill-b", SpawnMode::Resume, Outcome::Success, MIN_SAMPLE, 60));
        for c in &cards {
            append_catalog_line(cat.path(), c).unwrap();
        }
        let profiles = read_skill_profiles_at(cat.path());
        let a = profiles.iter().find(|p| p.skill == "skill-a").expect("skill-a");
        let b = profiles.iter().find(|p| p.skill == "skill-b").expect("skill-b");
        // Within-skill: opposite verdicts. If arms mixed cross-skill they'd both cancel.
        assert_eq!(a.fresh_vs_resume.verdict, AbVerdict::FreshFavored, "skill-a judged on its own arms");
        assert_eq!(b.fresh_vs_resume.verdict, AbVerdict::ResumeFavored, "skill-b judged on its own arms");
    }

    // SV5: the A/B tokens REUSE the existing telemetry (from_snapshot) — a stamp
    // without a tokens figure preserves it; an explicit stamp overrides.
    #[test]
    fn sv5_tokens_reuse_telemetry_and_stamp_can_override() {
        // A record whose tokens came from the tab's agent-tokens telemetry (5000).
        let card = v2_card("t", "builder", SpawnMode::Fresh, Some(Outcome::Success), 1, Some(5000), None);
        let base_stamp = V2Stamp { skill: Some("builder".into()), prompt: Some("p".into()), ..Default::default() };
        assert_eq!(
            card.clone().with_v2(base_stamp).tokens,
            Some(5000),
            "telemetry tokens preserved when the stamp omits them"
        );
        let override_stamp =
            V2Stamp { skill: Some("builder".into()), prompt: Some("p".into()), tokens: Some(9999), ..Default::default() };
        assert_eq!(card.with_v2(override_stamp).tokens, Some(9999), "an explicit stamp figure overrides");
    }

    // ----- SC1 (#39): catalogue mutations — event-sourced fold 2-axes -----------

    /// A content record (retire) for `skill` at prompt version `ver` with prompt `pN`.
    fn content_rec(id: &str, skill: &str, ver: u32) -> CatalogCard {
        let mut c = v2_card(id, skill, SpawnMode::Fresh, Some(Outcome::Success), 1, None, None);
        c.prompt = Some(format!("p{ver}"));
        c.prompt_version = Some(ver);
        c
    }

    // SC1: a retire record omits the `kind` key (byte-identical); an edit carries it.
    #[test]
    fn sc1_retire_record_omits_the_kind_key() {
        let card = v2_card("t", "builder", SpawnMode::Fresh, Some(Outcome::Success), 1, None, None);
        assert_eq!(card.kind, RecordKind::Retire, "default kind is retire");
        let json = encode_catalog_line(&card);
        assert!(!json.contains("\"kind\""), "a retire record omits the kind key (byte-identical): {json}");
        let edit = CatalogCard { kind: RecordKind::Edit, skill: Some("x".into()), ..Default::default() };
        assert!(encode_catalog_line(&edit).contains(r#""kind":"edit""#), "an edit carries kind:edit");
    }

    // SC1 CONTENT axis: the later-APPENDED record wins, NOT the later timestamp — an
    // edit appended after a retire wins even with an older `retired_at`.
    #[test]
    fn sc1_content_is_latest_append_not_latest_timestamp() {
        let cat = TmpCatalog::new();
        let mut retire = content_rec("i1", "rustsmith", 1);
        retire.retired_at = 999; // a LATER timestamp…
        let edit = CatalogCard {
            skill: Some("rustsmith".into()),
            prompt: Some("edited prompt".into()),
            prompt_version: Some(2),
            kind: RecordKind::Edit,
            schema_version: Some(2),
            retired_at: 1, // …but an OLDER timestamp, appended AFTER.
            ..Default::default()
        };
        append_catalog_line(cat.path(), &retire).unwrap();
        append_catalog_line(cat.path(), &edit).unwrap();
        let p = &read_skill_profiles_at(cat.path())[0];
        assert_eq!(p.prompt.as_deref(), Some("edited prompt"), "the later-APPENDED edit wins (append order)");
        assert_eq!(p.prompt_version, Some(2));
    }

    // SC1 VISIBILITY axis (borne 5, the ⭐ key test): delete tombstones (sticky); an
    // edit after delete does NOT resurrect; a retire after delete does NOT resurrect;
    // ONLY an explicit restore brings it back.
    #[test]
    fn sc1_delete_is_sticky_only_restore_resurrects() {
        let cat = TmpCatalog::new();
        append_catalog_line(cat.path(), &content_rec("i1", "s", 1)).unwrap();
        assert_eq!(read_skill_profiles_at(cat.path()).len(), 1, "present before delete");

        append_catalog_line(cat.path(), &visibility_record("s", RecordKind::Delete, 2)).unwrap();
        assert!(read_skill_profiles_at(cat.path()).is_empty(), "delete tombstones the skill");

        // ⭐ an EDIT after delete → still hidden (edit never touches visibility).
        let mut edit = content_rec("x", "s", 2);
        edit.kind = RecordKind::Edit;
        append_catalog_line(cat.path(), &edit).unwrap();
        assert!(read_skill_profiles_at(cat.path()).is_empty(), "edit after delete does NOT resurrect (borne 5)");

        // ⭐ a normal RETIRE after delete (same skill name) → still hidden (the
        // name-recurrence footgun is closed).
        append_catalog_line(cat.path(), &content_rec("i2", "s", 3)).unwrap();
        assert!(read_skill_profiles_at(cat.path()).is_empty(), "retire after delete does NOT resurrect");

        // restore → visible again, with the LATEST content (the post-delete retire v3).
        append_catalog_line(cat.path(), &visibility_record("s", RecordKind::Restore, 4)).unwrap();
        let after = read_skill_profiles_at(cat.path());
        assert_eq!(after.len(), 1, "restore is the ONLY resurrection path");
        assert_eq!(after[0].prompt_version, Some(3), "restored with the latest content (append order)");
    }

    // SC1 plan_edit: absent fields carry from latest, version bumps, CF1 + optimistic
    // concurrency gates, NotFound on a missing skill.
    #[test]
    fn sc1_plan_edit_carries_bumps_and_gates() {
        let mut latest = content_rec("i1", "rustsmith", 4);
        latest.prompt = Some("base prompt".into());
        latest.specialty = Some("rust".into());
        latest.conventions = vec!["A.md".into()];
        latest.tools = vec!["cargo".into()];
        // edit only specialty → prompt/conventions/tools carried; version 4→5.
        let body = EditBody { specialty: Some("rust daemon".into()), ..Default::default() };
        let rec = plan_edit(Some(&latest), "rustsmith", &body, 100).unwrap();
        assert_eq!(rec.kind, RecordKind::Edit);
        assert_eq!(rec.specialty.as_deref(), Some("rust daemon"), "specialty edited");
        assert_eq!(rec.prompt.as_deref(), Some("base prompt"), "prompt carried from latest");
        assert_eq!(rec.conventions, vec!["A.md"], "conventions carried");
        assert_eq!(rec.tools, vec!["cargo"], "tools carried");
        assert_eq!(rec.prompt_version, Some(5), "version bumped 4→5");
        assert_eq!(rec.usage_count, None, "an edit is not a metric data-point");
        // CF1 (borne 4): an explicit empty prompt → EmptyProfile.
        let empty = EditBody { prompt: Some("  ".into()), ..Default::default() };
        assert_eq!(plan_edit(Some(&latest), "rustsmith", &empty, 100), Err(EditError::EmptyProfile));
        // Optimistic concurrency (borne 3): a stale expected version → Conflict.
        let stale = EditBody { prompt: Some("new".into()), prompt_version: Some(3), ..Default::default() };
        assert_eq!(plan_edit(Some(&latest), "rustsmith", &stale, 100), Err(EditError::Conflict));
        let ok = EditBody { prompt: Some("new".into()), prompt_version: Some(4), ..Default::default() };
        assert!(plan_edit(Some(&latest), "rustsmith", &ok, 100).is_ok(), "matching version → ok");
        // No such skill → NotFound.
        assert_eq!(plan_edit(None, "ghost", &body, 100), Err(EditError::NotFound));
    }

    // SC1b (#39): the default read-model HIDES tombstoned skills; `read_all` surfaces
    // them with `deleted:true` (real camelCase marker), visible ones with `deleted`
    // skipped (frozen contract unchanged).
    #[test]
    fn sc1b_read_all_marks_deleted_and_default_hides_them() {
        let cat = TmpCatalog::new();
        append_catalog_line(cat.path(), &content_rec("v1", "visible", 1)).unwrap();
        append_catalog_line(cat.path(), &content_rec("d1", "gone", 1)).unwrap();
        append_catalog_line(cat.path(), &visibility_record("gone", RecordKind::Delete, 2)).unwrap();

        // Default: only the visible skill, never marked deleted.
        let vis = read_skill_profiles_at(cat.path());
        assert_eq!(vis.len(), 1, "default hides the tombstoned skill");
        assert_eq!(vis[0].skill, "visible");
        assert!(!vis[0].deleted);

        // include-deleted: BOTH, the tombstoned one flagged.
        let all = read_skill_profiles_all_at(cat.path());
        assert_eq!(all.len(), 2, "include-deleted surfaces the tombstone too");
        let gone = all.iter().find(|p| p.skill == "gone").expect("tombstone present");
        assert!(gone.deleted, "the tombstoned skill is marked deleted:true");
        // Its profile still folds (the Restore UI shows it).
        assert_eq!(gone.prompt.as_deref(), Some("p1"));
        let v = all.iter().find(|p| p.skill == "visible").expect("visible present");
        assert!(!v.deleted);

        // Serialization: `deleted:true` on the tombstone; the key is ABSENT on visible
        // (the frozen contract shape is unchanged for live cards).
        assert!(serde_json::to_string(gone).unwrap().contains(r#""deleted":true"#), "camelCase marker present");
        assert!(!serde_json::to_string(v).unwrap().contains("deleted"), "no marker on a visible card");
    }
}
