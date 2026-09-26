// SPDX-License-Identifier: MPL-2.0

//! `catalog` — the PROFIL catalogue (v2) + `spawn --from-skill`.
//!
//! In fungal mode the ORGANISATION is gone: what survives is a catalogue of reusable
//! PROFILES — one skill, its distilled prompt, its tools, and its MEASURED efficiency.
//! The loop that improves the prompt (bilan → éval-à-3 → improved prompt → re-spawn) is
//! the heart of it.
//!
//! # Scope: the v2 half of the MX catalogue
//!
//! Ported from MX `d08b0f9e` (`src/cli/catalog.rs`), which carried TWO models side by
//! side:
//!
//! - **v1 `CatalogCard`** = a card of ORGANISATION (`assignment`, `specialty`,
//!   `orchestrator`, `objective`, `current_task_log`, and the `set-card`-family verbs).
//!   That model was retired by the PO — NOT ported.
//! - **v2 `SkillProfile`** = a PROFILE. Ported here.
//!
//! # Why [`CatalogRecord`] has no v1 field
//!
//! serde IGNORES unknown JSON keys, so a `catalog.jsonl` written by the v1/v2 MX
//! build still parses into this narrower struct — the organisation fields are read
//! and dropped. The only WRITER is [`run_retire`] (`catalog retire`), and that path
//! stamps a v2 profile (`skill` + `prompt`) — so declaring v1 fields nobody reads
//! would be write surface with no reader.
//!
//! # The write-path (the loop that improves the prompt)
//!
//! `catalog retire` takes a [`V2Stamp`] (the distilled profile + this instance's
//! telemetry) plus the agent's [`Bilan`] and runs [`perform_retire`]: append the
//! record, RE-READ it, and gate the close on that read-back. The read-back is the
//! PROOF — not "I wrote it" — so a profile that didn't land keeps its tab.
//!
//! # The read-model
//!
//! Records fold BY SKILL NAME (the `fold_key`) into one mode-agnostic profile plus
//! per-mode metrics; `fresh_vs_resume` is DERIVED at read, never stored (the read-only
//! discipline). v1 records are QUARANTINED (filtered by `is_v2`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// The two partition/outcome enums the metrics rest on.
// ---------------------------------------------------------------------------

/// How an instance of a skill was BORN — the per-instance PARTITION key for the v2
/// metrics.
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

/// The retire OUTCOME — v2. Derived from the éval-à-3, NOT a self-report. The default
/// is `Problem` — a conservative bar: nothing is a success until the eval says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    #[default]
    Problem,
}

/// The event-sourced record TYPE — the fold runs on TWO independent axes:
/// - CONTENT (profile) = latest-append-wins over `{Retire, Edit}` records;
/// - VISIBILITY = last-wins over `{Delete, Restore}` records ONLY.
///
/// `Retire` is the default, so a record with no `kind` key reads as a retire — the
/// v1 records this port reads are untouched by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// An agent retirement — a metric data-point + a profile snapshot. The default.
    #[default]
    Retire,
    /// A profile EDIT — new prompt/specialty/conventions. Touches CONTENT.
    Edit,
    /// A tombstone — hides the skill from the read-model (STICKY). Touches VISIBILITY.
    Delete,
    /// An explicit un-tombstone — the ONLY resurrection path. Touches VISIBILITY.
    Restore,
}

impl RecordKind {
    /// A CONTENT record — carries profile fields (`Retire` or `Edit`).
    const fn is_content(self) -> bool {
        matches!(self, Self::Retire | Self::Edit)
    }
    /// A VISIBILITY record — flips the tombstone (`Delete` or `Restore`).
    const fn is_visibility(self) -> bool {
        matches!(self, Self::Delete | Self::Restore)
    }
}

// ---------------------------------------------------------------------------
// The v2 stamp + the structured BILAN — the improvement loop's inputs.
// ---------------------------------------------------------------------------

/// The v2 stamp an orchestrator applies at retire time.
///
/// The distilled PROFILE fields (`skill` name + prompt/tools/patterns) plus this
/// instance's per-mode telemetry (`spawn_mode`, `outcome`, `tokens`, `cost`).
/// Everything is optional, so a legacy (v1) retire — which carries none of it —
/// stays byte-identical. `specialty`/`conventions` already live on the record and are
/// reused.
///
/// Ported as the WRITE-side contract of the v2 record: the retired-card path (out of
/// this port's scope) deserialises it from a retire body. It is kept here so the
/// on-disk v2 shape has ONE definition shared by reader and writer.
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

/// The structured BILAN an agent produces at retire — a RETROSPECTIVE ON ITS PROMPT,
/// not on the precise task.
///
/// It replaces the 1-line `lastMission`: instead of "what I did", it captures what
/// the agent learned about its ROLE/PROMPT and how the prompt should change — the raw
/// material for the improved prompt and the profile. Every field is GENERALISABLE
/// (about the base prompt/context), never the run's precise facts — those stay in the
/// objective/task log, untouched. All fields optional so a bilan is only as full as
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
        self.learned.is_empty()
            && self.problems.is_empty()
            && self.add_directives.is_empty()
            && self.drop_directives.is_empty()
    }

    /// A compact one-line summary — the back-fill for the legacy `lastMission` slot so
    /// consumers that still read it get a readable digest of the structured bilan.
    #[must_use]
    pub fn one_line(&self) -> String {
        let seg = |label: &str, items: &[String]| (!items.is_empty()).then(|| format!("{label}: {}", items.join("; ")));
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

/// The `catalog retire` input.
///
/// The orchestrator's [`V2Stamp`] (flattened at the top level) plus the agent's
/// [`Bilan`] — the ONE definition of the retire body, shared by [`run_retire`] and the
/// eventual daemon route.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetireRequest {
    /// The agent's structured bilan — the retrospective on its PROMPT. Absent or empty
    /// ⇒ nothing is recorded (the profile stands, telemetry-only retire).
    #[serde(default)]
    pub bilan: Option<Bilan>,
    /// The éval-à-3 votes (agent / orchestrator / Olympe). Absent ⇒ the conservative
    /// all-abstain default (silence never improves).
    #[serde(default)]
    pub votes: EvalVotes,
    /// The PRECISE run's concrete values that must NOT leak into the derived prompt —
    /// the anti-over-fit ban's input. Absent ⇒ nothing is banned.
    #[serde(default)]
    pub task_literals: Vec<String>,
    #[serde(flatten)]
    pub stamp: V2Stamp,
}

impl RetireRequest {
    /// The record to archive: a v2 PROFILE (the stamp) carrying the bilan, keyed by the
    /// retired tab's `id`.
    ///
    /// This is where the éval-à-3 is WIRED into the retire path: when a non-empty bilan
    /// is present and the operator did NOT hand an explicit `--prompt`, [`evaluate`] runs
    /// against `base` (the skill's current folded profile) and its decision is APPLIED —
    /// `Improved` ⇒ the derived prompt + a bumped `prompt_version`, `StatuQuo` ⇒ the base
    /// prompt kept — and its report is ARCHIVED on the record. Without `base` (a skill's
    /// first record) the eval starts from the empty prompt, so it can only ADD.
    ///
    /// An explicit `--prompt` stays the manual escape hatch: it short-circuits the eval,
    /// so a hand-written prompt is never overwritten by a derivation.
    ///
    /// A request with no `skill` produces a record that FAILS the v2 completeness gate
    /// (and so stays out of the read-model) — [`perform_retire`] then keeps the tab
    /// rather than closing a profile-less retire. A retire that names no skill has no
    /// place in the v2 model, so it is quarantined rather than half-recorded.
    #[must_use]
    fn into_record(mut self, id: &str, retired_at: u64, base: Option<&SkillProfile>) -> CatalogRecord {
        let mut record = CatalogRecord {
            id: id.to_string(),
            retired_at,
            ..CatalogRecord::default()
        };
        let base_prompt = base.and_then(|p| p.prompt.clone()).unwrap_or_default();
        let base_version = base.and_then(|p| p.prompt_version);
        // An explicit prompt is the manual escape hatch: no derivation.
        let explicit = self.stamp.prompt.as_deref().is_some_and(|p| !p.trim().is_empty());
        if let Some(bilan) = self.bilan.take().filter(|b| !b.is_empty()) {
            if !explicit {
                let result = evaluate(&EvalInput {
                    base_prompt: base_prompt.clone(),
                    bilan: bilan.clone(),
                    task_literals: std::mem::take(&mut self.task_literals),
                    votes: self.votes,
                });
                match result.report.decision {
                    EvalDecision::Improved => {
                        self.stamp.prompt = Some(result.resulting_prompt);
                        // The PROFILE's next version — previous + 1, min 1 on a first record.
                        self.stamp.prompt_version = Some(base_version.unwrap_or(0) + 1);
                    }
                    EvalDecision::StatuQuo => {
                        // The original prompt stands (and its version is preserved).
                        self.stamp.prompt = Some(base_prompt);
                        self.stamp.prompt_version = base_version.or(self.stamp.prompt_version);
                    }
                }
                record.eval = Some(result.report);
            }
            record = record.with_bilan(bilan);
        }
        record.with_v2(self.stamp)
    }
}

// ---------------------------------------------------------------------------
// The éval-à-3: agent(bilan) + orchestrator + Olympe converge on the IMPROVED prompt
// (consensus) or keep the original (dissent → statu quo). The `outcome` is DERIVED
// from the eval, never self-reported; anti-over-fit is ENFORCED here (a mechanical ban
// on task LITERALS + a test-nouvelle-tâche), so the improved prompt holds on a NEW
// task rather than memorising the last one.
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
    /// Judges the instance's RUN a success (the outcome signal — NOT self-report).
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

/// The inputs to the éval-à-3, produced at retire before catalogage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalInput {
    /// The base (current) prompt the eval may improve.
    #[serde(default)]
    pub base_prompt: String,
    /// The agent's bilan — its `add_directives` are the proposed prompt changes.
    #[serde(default)]
    pub bilan: Bilan,
    /// The PRECISE task's concrete values that must NOT leak into the prompt. The
    /// live path fills these from the tab's objective + current-task log; here they
    /// are an input, so the ban is ENFORCED by the daemon, never trusted from a
    /// declared-clean set.
    #[serde(default)]
    pub task_literals: Vec<String>,
    /// The three evaluators' votes.
    #[serde(default)]
    pub votes: EvalVotes,
}

/// An ADDED directive's scope — the eval's neutral judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectiveScope {
    /// Generalisable — holds on a new task. Eligible for the improved prompt.
    General,
    /// Task-specific — carries the past run's specifics; would over-fit the prompt.
    TaskSpecific,
}

/// One added directive tagged with its scope (part of the EVAL output).
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

/// The éval-à-3 OUTPUT, stored on the v2 record.
///
/// The per-directive verdicts, the anti-over-fit findings, the traced decision, and the
/// DERIVED outcome. The resulting prompt is applied to the record's `prompt`, not
/// duplicated here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalReport {
    pub decision: EvalDecision,
    /// The outcome DERIVED from the three `run_ok` votes (majority), not self-reported.
    pub outcome: Outcome,
    /// Per-added-directive general|task-specific verdicts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directive_verdicts: Vec<DirectiveVerdict>,
    /// Task literals that leaked into a proposed directive (a ban trigger).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaked_literals: Vec<String>,
    /// Added directives that are task-specific (fail the test-nouvelle-tâche).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_specific_directives: Vec<String>,
    /// A human-readable trace of why the decision landed as it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<String>,
}

/// The result of the éval-à-3: the report to store + the resulting prompt to apply.
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

/// Run the a-priori éval-à-3. PURE.
///
/// Given the base prompt, the agent's bilan, the task's literals, and the three votes,
/// it decides improved-vs-statu-quo, derives the outcome, tags each directive, and
/// enforces the anti-over-fit gate.
///
/// - **FN1** tags each ADD directive `General` / `TaskSpecific` — task-specific ⇔ it
///   contains a task literal (a concrete past-run value).
/// - **FN2 E3b (ban literals)**: any task literal appearing in a directive is leaked.
/// - **FN2 E3c (test-nouvelle-tâche)**: any task-specific directive would not hold on
///   a new task.
/// - **Decision**: `Improved` iff all three approve AND no leak AND no task-specific
///   directive; otherwise `StatuQuo` (the veto beats a rubber-stamp consensus).
/// - **Outcome**: DERIVED from the three `run_ok` votes (majority), never self-report.
#[must_use]
pub fn evaluate(input: &EvalInput) -> EvalResult {
    let literals: Vec<&str> = input
        .task_literals
        .iter()
        .map(String::as_str)
        .filter(|l| is_meaningful_literal(l))
        .collect();
    let contains_literal = |text: &str| {
        let hay = text.to_lowercase();
        literals
            .iter()
            .find(|l| hay.contains(&l.to_lowercase()))
            .map(|l| (*l).to_string())
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
        let scope = if hit.is_some() {
            DirectiveScope::TaskSpecific
        } else {
            DirectiveScope::General
        };
        verdicts.push(DirectiveVerdict {
            directive: d.clone(),
            scope,
        });
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
    let outcome = if run_oks >= 2 {
        Outcome::Success
    } else {
        Outcome::Problem
    };

    let mut rationale = vec![format!(
        "prompt approvals {approvals}/3; run_ok {run_oks}/3; leaked_literals {}; task_specific {}",
        leaked.len(),
        task_specific.len()
    )];

    let (decision, resulting_prompt) = if unanimous && clean {
        rationale.push("consensus + anti-over-fit clean → improved prompt adopted".into());
        (
            EvalDecision::Improved,
            apply_directives(&input.base_prompt, &input.bilan),
        )
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
        report: EvalReport {
            decision,
            outcome,
            directive_verdicts: verdicts,
            leaked_literals: leaked,
            task_specific_directives: task_specific,
            rationale,
        },
        resulting_prompt,
    }
}

/// Apply the bilan's prompt directives to the base prompt: drop lines matching a
/// `−consigne`, then append each `+consigne` as a new line. A minimal textual model of
/// "the improved prompt" (the eval already vetted the additions).
fn apply_directives(base: &str, bilan: &Bilan) -> String {
    let mut lines: Vec<String> = base
        .lines()
        .filter(|l| {
            !bilan
                .drop_directives
                .iter()
                .any(|d| !d.trim().is_empty() && l.contains(d.trim()))
        })
        .map(str::to_string)
        .collect();
    for add in &bilan.add_directives {
        if !add.trim().is_empty() {
            lines.push(add.clone());
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// The on-disk record (the subset the v2 read-model folds).
// ---------------------------------------------------------------------------

/// One line of `catalog.jsonl`, narrowed to the fields the v2 read-model and the
/// `--resume` baseline need.
///
/// The v1/organisation fields (`assignment`, `orchestrator`, `objective`,
/// `current_task_log`, …) are deliberately NOT declared: serde ignores unknown keys,
/// so an existing catalogue still parses and those fields are simply dropped. Nothing
/// in this port writes a record, so declaring them would be write surface with no
/// reader (see the module docs).
// `Serialize` is production, not test-only: `catalog retire` WRITES a record through
// [`encode_catalog_line`], so the on-disk shape has ONE definition shared by the
// read-model and the write-path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRecord {
    /// The archived tab's id — the key [`read_back`] looks up (append-only ⇒ the LAST
    /// line wins). Required: [`perform_retire`]'s gate refuses an id-less archive.
    #[serde(default)]
    id: String,
    #[serde(default)]
    specialty: Option<String>,
    #[serde(default)]
    conventions: Vec<String>,
    #[serde(default)]
    usage_count: Option<u64>,
    /// The A/B BASELINE: the archived agent session + kind. Excluded from the profile
    /// fold (so `SkillProfile` stays A/B-isolated) but read back by
    /// [`resolve_skill_baseline`] for a `--resume` spawn.
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    agent_kind: Option<String>,
    // ----- v2 profile + telemetry -----
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    prompt_version: Option<u32>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
    /// How this instance was born — the metric PARTITION key.
    #[serde(default)]
    spawn_mode: Option<SpawnMode>,
    /// The retire outcome (from the éval-à-3, not self-report).
    #[serde(default)]
    outcome: Option<Outcome>,
    #[serde(default)]
    tokens: Option<u64>,
    #[serde(default)]
    cost: Option<f64>,
    /// How hard this instance judged its task — telemetry, stamp-only (there is no
    /// per-tab difficulty to inherit). Declared so the [`V2Stamp`] write shape has
    /// exactly one definition.
    #[serde(default)]
    difficulty: Option<u8>,
    /// `Some(2)` = a v2 record (in the skill read-model); `None`/`Some(1)` = a v1
    /// legacy record, QUARANTINED from the v2 read-model.
    #[serde(default)]
    schema_version: Option<u32>,
    /// The record TYPE. `Retire` is the default, so a v1 record with no `kind` reads
    /// as a retire.
    #[serde(default)]
    kind: RecordKind,
    /// The agent's structured [`Bilan`] — the retrospective on its PROMPT that feeds
    /// the improved prompt (the raw material of the loop). Captured at retire, before
    /// the close.
    #[serde(default)]
    bilan: Option<Bilan>,
    /// The éval-à-3 [`EvalReport`] — the trace of "we improved / we kept", so the
    /// decision that shaped `prompt` is AUDITABLE after the fact. `None` on a
    /// telemetry-only retire (no bilan) or a manual `--prompt` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eval: Option<EvalReport>,
    retired_at: u64,
}

impl CatalogRecord {
    /// A v2 record — folded into the skill read-model. `schema_version == Some(2)`.
    /// A v1/legacy record (`None`/`Some(1)`) is QUARANTINED from the v2 read-model.
    #[must_use]
    fn is_v2(&self) -> bool {
        self.schema_version == Some(2)
    }

    /// Does this record carry a NON-EMPTY `skill`? A `schema_version:2` record without
    /// a skill is an incomplete profile (never closed, quarantined from the read-model).
    #[must_use]
    fn has_skill(&self) -> bool {
        self.skill.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    /// The fold key: the proper `skill` name, trimmed. Never the freeform tab name nor
    /// the tab-id. The callers gate on [`Self::has_skill`] first, so a skill-less
    /// record never reaches a fold — it is quarantined as an incomplete profile.
    #[must_use]
    fn fold_key(&self) -> String {
        self.skill.as_deref().unwrap_or_default().trim().to_string()
    }

    /// Does this record carry a NON-EMPTY `prompt`? The other half of the v2
    /// completeness bar — a profile with no distilled prompt can't be re-seeded.
    #[must_use]
    fn has_prompt(&self) -> bool {
        self.prompt.as_deref().is_some_and(|p| !p.trim().is_empty())
    }

    /// The WRITE gate: is this archived record complete enough to close behind?
    ///
    /// - **v2** (CF1): a NON-EMPTY `skill`. The prompt is deliberately NOT required:
    ///   the eval can legitimately fail to produce one (every directive vetoed on a
    ///   skill's first retire, no `--prompt`), and there is then nothing to distill
    ///   rather than something missing. Refusing the close would hold a tab forever
    ///   over a profile that cannot exist. Such a record is archived — it keeps its
    ///   telemetry — but the read-model quarantines it (see [`read_skill_profiles_at`]),
    ///   so it never becomes a profile and can never blank one.
    ///   `session_id` is NOT required — the v2 baseline is A/B-isolated and optional.
    /// - **v1**: WHEN the tab carried a live session (`had_session`), the archive must
    ///   carry the `session_id` — a lost existing session is an incomplete archive.
    #[must_use]
    fn is_complete(&self, had_session: bool) -> bool {
        if self.id.is_empty() {
            return false;
        }
        if self.is_v2() {
            return self.has_skill();
        }
        !had_session || self.session_id.is_some()
    }

    /// Promote this record to a v2 PROFILE by stamping the orchestrator's [`V2Stamp`]
    /// — the distilled profile plus this instance's per-mode telemetry. Sets
    /// `schema_version` to 2 (the read-model's opt-in). The baseline
    /// (`session_id`/`agent_kind`) is untouched: it stays A/B-isolated.
    #[must_use]
    fn with_v2(mut self, stamp: V2Stamp) -> Self {
        self.skill = stamp.skill.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.prompt_version = stamp.prompt_version;
        self.prompt = stamp.prompt;
        self.tools = stamp.tools;
        self.patterns = stamp.patterns;
        // Keep a spawn-time mode (seeded on the tab) unless the stamp overrides it.
        if stamp.spawn_mode.is_some() {
            self.spawn_mode = stamp.spawn_mode;
        }
        self.outcome = stamp.outcome;
        // Telemetry sourced elsewhere (the tab) is kept unless the stamp is explicit.
        if stamp.tokens.is_some() {
            self.tokens = stamp.tokens;
        }
        if stamp.cost.is_some() {
            self.cost = stamp.cost;
        }
        if stamp.difficulty.is_some() {
            self.difficulty = stamp.difficulty;
        }
        self.schema_version = Some(2);
        self
    }

    /// Attach the agent's structured [`Bilan`] — captured at retire, BEFORE the close.
    /// An empty bilan is a no-op: nothing to record, so the record is untouched.
    #[must_use]
    fn with_bilan(mut self, bilan: Bilan) -> Self {
        if !bilan.is_empty() {
            self.bilan = Some(bilan);
        }
        self
    }
}

/// A test/ops override of the catalogue path.
///
/// The repo's `set_*_path` seam (cf. `cli::team::set_blackboard_path`), so a test can
/// point the read-model at a temp file without touching the developer's real catalogue.
/// `None` restores the default.
pub fn set_catalog_path(path: Option<PathBuf>) {
    if let Ok(mut g) = CATALOG_OVERRIDE.write() {
        *g = path;
    }
}

/// The catalogue-file override, set by [`set_catalog_path`].
static CATALOG_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// The catalogue file: `<state>/tab-atelier/catalog.jsonl`. Honors
/// `TAB_ATELIER_CATALOG_PATH` (an ops seam) then the test override.
#[must_use]
pub fn catalog_path() -> PathBuf {
    if let Some(p) = CATALOG_OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return p;
    }
    if let Ok(p) = std::env::var("TAB_ATELIER_CATALOG_PATH") {
        return PathBuf::from(p);
    }
    crate::platform::state_base_dir()
        .join("tab-atelier")
        .join("catalog.jsonl")
}

/// Parse a catalogue file body into records, skipping blank / unparseable lines
/// (a half-written line from a racing appender is dropped, not fatal — same
/// tolerance as the task queue's `parse_tasks`).
#[must_use]
fn parse_catalog(body: &str) -> Vec<CatalogRecord> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CatalogRecord>(l).ok())
        .collect()
}

/// Every record of the catalogue (v1 + v2, unfiltered) — the `--resume` baseline
/// lookup needs the raw records, since the read-model drops the baseline.
#[must_use]
fn read_catalog_records_at(path: &Path) -> Vec<CatalogRecord> {
    parse_catalog(&std::fs::read_to_string(path).unwrap_or_default())
}

/// [`read_catalog_records_at`] against the live [`catalog_path`].
#[must_use]
fn read_catalog_records() -> Vec<CatalogRecord> {
    read_catalog_records_at(&catalog_path())
}

/// One catalog record as a JSONL line (trailing newline included).
///
/// A serialisation failure can only mean a non-serialisable field was added; falling
/// back to `{}` writes an unparseable line the reader DROPS, which the retire gate
/// then catches as an incomplete read-back (the tab is kept, never closed blind).
#[must_use]
pub fn encode_catalog_line(record: &CatalogRecord) -> String {
    serde_json::to_string(record).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// Append one record to the catalogue (create + append, line-atomic like the swamp /
/// task producer). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / open / write I/O error.
pub fn append_catalog_line(path: &Path, record: &CatalogRecord) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_catalog_line(record).as_bytes())
}

/// RE-READ the catalogue for the LATEST archived record of `id`.
///
/// Append-only ⇒ last write wins; `None` when the id was never archived. This is the
/// read-back [`perform_retire`] gates on — proof the write LANDED, not "I wrote it".
#[must_use]
pub fn read_back(path: &Path, id: &str) -> Option<CatalogRecord> {
    parse_catalog(&std::fs::read_to_string(path).ok()?)
        .into_iter()
        .rev()
        .find(|c| c.id == id)
}

// ---------------------------------------------------------------------------
// The v2 SKILL read-model: fold records BY SKILL NAME into one mode-agnostic profile
// + per-mode metrics + a DERIVED fresh-vs-resume compare.
//
// The mode PARTITIONS the metrics, NOT the profile (1 skill = 1 skill however its
// instances were born). `fresh_vs_resume` is DERIVED at read, never stored.
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
    fn of(instances: &[CatalogRecord], mode: SpawnMode) -> Self {
        let arm: Vec<&CatalogRecord> = instances.iter().filter(|c| c.spawn_mode == Some(mode)).collect();
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

/// The minimum instances PER ARM before the A/B yields a directional verdict.
///
/// Below this, the verdict is [`AbVerdict::InsufficientSample`] (the raw deltas are
/// still surfaced, but never interpreted).
///
/// ponytail: a heuristic floor for a dogfood-scale ledger, tunable — not a power
/// analysis. The upgrade path is a proper significance test once N is large.
pub const MIN_SAMPLE: u64 = 3;

/// The dead-zone on the delivery delta: below this the arms are called
/// [`AbVerdict::Inconclusive`] rather than favouring either. ponytail: heuristic.
const DELIVERY_DEAD_ZONE: f64 = 0.15;

/// The DIRECTIONAL A/B verdict — never a per-task pass/fail, always a trend surfaced
/// WITH its sample size (`fresh_n`/`resume_n`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AbVerdict {
    /// An arm has fewer than [`MIN_SAMPLE`] instances → conclude NOTHING.
    #[default]
    InsufficientSample,
    /// Enough data, but the delivery delta is within the dead-zone → no clear winner.
    Inconclusive,
    /// Fresh delivers better on this skill (directional).
    FreshFavored,
    /// Resume delivers better on this skill (directional).
    ResumeFavored,
}

/// The fresh-vs-resume comparison — DERIVED at read, never stored.
///
/// The raw `delivery_delta` / `tokens_ratio` / `cost_ratio` are `None` when a side
/// lacks the data. The verdict is GUARDED (min-sample) and always computed WITHIN one
/// skill's own instances (structural: the read-model folds by skill).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreshVsResume {
    /// `fresh.success_rate − resume.success_rate` (positive ⇒ fresh delivers better).
    pub delivery_delta: Option<f64>,
    /// `fresh.tokensAvg / resume.tokensAvg`.
    pub tokens_ratio: Option<f64>,
    /// `fresh.costAvg / resume.costAvg`.
    pub cost_ratio: Option<f64>,
    /// The directional verdict (never pass/fail), guarded by [`MIN_SAMPLE`].
    pub verdict: AbVerdict,
    /// The sample size the verdict rests on, always surfaced.
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
        // Min-sample (G1) → directional verdict surfaced with n. Both arms are this ONE
        // skill's arms (the caller folded by skill).
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
    /// The proper skill NAME — the fold key.
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
    /// The latest record's timestamp (the profile winner).
    pub retired_at: u64,
    pub metrics: Metrics,
    pub fresh_vs_resume: FreshVsResume,
    /// `true` only for a TOMBSTONED skill surfaced via `?includeDeleted`. Skipped when
    /// `false`, so the normal read-model shape (which never lists deleted skills
    /// anyway) is byte-unchanged.
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
    /// `{Retire|Edit}` record in APPEND ORDER (not a timestamp compare: an `Edit` has
    /// no retire time and clocks skew; append order is the event-sourced authority).
    /// Metrics aggregate the retire data-points (mutation records carry no
    /// `spawn_mode`, so they're inert for the per-mode metrics).
    fn fold(skill: String, records: &[CatalogRecord]) -> Self {
        // `records` is in append (file) order. The content winner is the LAST content
        // record. A visible group always has ≥1 content record; guard anyway.
        let Some(latest) = records.iter().rfind(|c| c.kind.is_content()) else {
            return Self {
                skill,
                ..Default::default()
            };
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
            metrics: Metrics {
                by_mode: ByMode { fresh, resume },
            },
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
/// Only COMPLETE v2 profiles fold in: a `schema:2` record without a non-empty skill
/// (an incomplete write a refused retire kept on disk) is quarantined too, so the
/// read-model never shows a half-built profile.
///
/// The VISIBILITY axis: a skill whose LAST `{Delete|Restore}` record is a `Delete` is
/// TOMBSTONED and filtered out (derived at read, never materialised). An `Edit` or a
/// `Retire` never flips visibility — resurrection is a `Restore` only.
#[must_use]
pub fn read_skill_profiles_at(path: &Path) -> Vec<SkillProfile> {
    use std::collections::BTreeMap;
    let mut by_skill: BTreeMap<String, Vec<CatalogRecord>> = BTreeMap::new();
    for c in read_catalog_records_at(path)
        .into_iter()
        // A v2 record with no prompt is not a profile: the eval had nothing to
        // distill. It is archived (its telemetry is worth keeping) but QUARANTINED
        // from the read-model here — and this filter matters, because `fold` copies
        // the prompt of the last content record: admitting a promptless one would
        // blank an existing profile's prompt.
        //
        // Visibility records are exempt: a tombstone is not a profile and carries no
        // prompt by nature, so requiring one would drop the tombstone and resurrect
        // the skill it hides.
        .filter(|c| c.is_v2() && c.has_skill() && (c.kind.is_visibility() || c.has_prompt()))
    {
        by_skill.entry(c.fold_key()).or_default().push(c);
    }
    by_skill
        .into_iter()
        .filter(|(_, group)| is_visible(group))
        .map(|(skill, group)| SkillProfile::fold(skill, &group))
        .collect()
}

/// VISIBILITY axis: a skill is visible unless its LAST visibility record
/// (`Delete`/`Restore`, in append order) is a `Delete`. No visibility record ⇒ visible
/// (the normal case). `records` is in append order.
fn is_visible(records: &[CatalogRecord]) -> bool {
    records
        .iter()
        .rev()
        .find(|c| c.kind.is_visibility())
        .is_none_or(|c| c.kind != RecordKind::Delete)
}

/// [`read_skill_profiles_at`] against the live [`catalog_path`] — the v2 `skills`
/// read-model of `GET /catalog/list`. READ-ONLY.
#[must_use]
pub fn read_skill_profiles() -> Vec<SkillProfile> {
    read_skill_profiles_at(&catalog_path())
}

/// The read-model INCLUDING tombstoned skills, each marked `deleted:true`.
///
/// Same fold as [`read_skill_profiles_at`] but WITHOUT the visibility filter, so the
/// dashboard can reach the Restore action (`?includeDeleted`). Visible skills are
/// byte-identical (`deleted` skipped). Path-injectable; READ-ONLY.
#[must_use]
pub fn read_skill_profiles_all_at(path: &Path) -> Vec<SkillProfile> {
    use std::collections::BTreeMap;
    let mut by_skill: BTreeMap<String, Vec<CatalogRecord>> = BTreeMap::new();
    for c in read_catalog_records_at(path)
        .into_iter()
        // A v2 record with no prompt is not a profile: the eval had nothing to
        // distill. It is archived (its telemetry is worth keeping) but QUARANTINED
        // from the read-model here — and this filter matters, because `fold` copies
        // the prompt of the last content record: admitting a promptless one would
        // blank an existing profile's prompt.
        //
        // Visibility records are exempt: a tombstone is not a profile and carries no
        // prompt by nature, so requiring one would drop the tombstone and resurrect
        // the skill it hides.
        .filter(|c| c.is_v2() && c.has_skill() && (c.kind.is_visibility() || c.has_prompt()))
    {
        by_skill.entry(c.fold_key()).or_default().push(c);
    }
    by_skill
        .into_iter()
        .map(|(skill, group)| {
            let deleted = !is_visible(&group);
            SkillProfile {
                deleted,
                ..SkillProfile::fold(skill, &group)
            }
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
// spawn --from-skill <name> [--task <t>] [--resume]: CREATE a real tab seeded from a
// skill's folded profile. Default = fresh+adapt (profile prompt + task overlay);
// `--resume` = the A/B baseline bench (resume the archived session). Matching is by
// the proper skill NAME, never a short-id/slug.
// ---------------------------------------------------------------------------

/// The fresh-spawn launcher: `claude` already in auto permission mode (mirrors
/// spawn-bot.sh, so a fresh agent doesn't stall on per-tool approvals).
pub const FRESH_LAUNCHER: &str = "claude --permission-mode auto";

/// Resolve a folded skill profile by its proper NAME.
///
/// Exact first, then case-insensitive. `None` when no skill matches. NEVER a short-id /
/// slug — a spawn matches the human-given skill name.
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
/// DROPS the baseline (A/B-isolated), so `--resume` looks it up on the records here.
/// A missing `agent_kind` defaults to `"claude"`. `None` when there's no session.
#[must_use]
fn resolve_skill_baseline(records: &[CatalogRecord], skill: &str) -> Option<(String, String)> {
    records
        .iter()
        .filter(|c| c.is_v2() && c.has_skill() && c.fold_key() == skill && c.session_id.is_some())
        .max_by_key(|c| c.retired_at)
        .map(|c| {
            (
                c.session_id.clone().unwrap_or_default(),
                c.agent_kind.clone().unwrap_or_else(|| "claude".to_string()),
            )
        })
}

/// The A/B baseline for `skill` against the LIVE catalogue — the `--resume` entry point.
///
/// The read-model drops the baseline (it stays A/B-isolated), so the raw records are
/// read here to find the latest retired instance's session.
#[must_use]
pub fn skill_baseline(skill: &str) -> Option<(String, String)> {
    resolve_skill_baseline(&read_catalog_records(), skill)
}

/// How a `spawn --from-skill` launches + what profile to seed on the new tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSpawnPlan {
    pub skill: String,
    /// `Fresh` (default) or `Resume` — the A/B arm the new tab belongs to.
    pub spawn_mode: SpawnMode,
    /// The launcher command to run in the new tab's shell.
    pub cmd: String,
    /// The prompt to send: fresh = distilled profile prompt + task overlay; resume =
    /// the task overlay only (the resumed session already carries its context).
    pub prompt: String,
    pub specialty: Option<String>,
    pub conventions: Vec<String>,
}

/// Build the spawn plan. PURE.
///
/// DEFAULT = fresh+adapt: the profile prompt + a `--task` overlay, `SpawnMode::Fresh`,
/// the [`FRESH_LAUNCHER`]. `resume` = the baseline bench: rebuild the resume command
/// for the baseline session, `SpawnMode::Resume` — erroring if the skill has no
/// baseline session to resume.
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
        let (sid, kind) =
            baseline.ok_or_else(|| format!("skill '{}' has no baseline session to --resume", profile.skill))?;
        let cmd = crate::build_agent_resume_command(kind, sid, None)
            .ok_or_else(|| format!("cannot build a resume command for agent kind '{kind}'"))?;
        (SpawnMode::Resume, cmd, overlay(""))
    } else {
        (
            SpawnMode::Fresh,
            FRESH_LAUNCHER.to_string(),
            overlay(profile.prompt.as_deref().unwrap_or_default()),
        )
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
// The retire write-path: archive → RE-READ → gate → de-register → close.
// ---------------------------------------------------------------------------

/// The verdict of a retire attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// Archived (read-back OK + complete), de-registered, and closed. Terminal.
    Retired,
    /// The archive write or its read-back was empty / incomplete → the close was NEVER
    /// reached, the tab is KEPT (the "RETIRE INCOMPLET" flag). Re-retire is replayable
    /// (idempotent up to the close).
    Incomplete(&'static str),
    /// Archive OK but a terminal step (de-register or shutdown) failed → the tab is
    /// KEPT and NOT a ghost: `shutdown` is the LAST effect, after the durable
    /// de-register, so the forbidden {closed BUT still registered} can't happen.
    CloseFailed,
}

/// The retire window: archive the record, RE-READ it, and — only on a complete
/// read-back — de-register then close. PURE + persist-gated + fully mockable.
///
/// Every effect is an INJECTED closure, so a test records the call + its ORDER without
/// touching a real tab (the same pattern as the repo's other `perform_*` windows). The
/// persist-gate is the [`read_back`] + [`CatalogRecord::is_complete`] check between the
/// archive and the close: a failed / empty / incomplete read-back means the close is
/// NEVER reached.
///
/// Order of effects: `write_catalog` → `read_back` → [gate] → `deregister` →
/// `shutdown`. `shutdown` (the only real-tab-touching seam) runs LAST, after the
/// durable de-register, so a failure never leaves {closed BUT still registered}.
pub fn perform_retire<Wc, Rb, Dr, Sd>(
    record: &CatalogRecord,
    ack_safe_to_close: bool,
    had_session: bool,
    write_catalog: Wc,
    read_back: Rb,
    deregister: Dr,
    shutdown: Sd,
) -> RetireOutcome
where
    Wc: FnOnce(&CatalogRecord) -> std::io::Result<()>,
    Rb: FnOnce(&str) -> Option<CatalogRecord>,
    Dr: FnOnce() -> std::io::Result<()>,
    Sd: FnOnce() -> std::io::Result<()>,
{
    // GATE 3a (fail-safe, cumulative + INDEPENDENT of the archive gate): no
    // `safe-to-close` ACK → NO close, the tab is kept.
    if !ack_safe_to_close {
        return RetireOutcome::Incomplete("no safe-to-close ACK — RETIRE INCOMPLET, tab kept");
    }
    // 1. Archive the record.
    if write_catalog(record).is_err() {
        return RetireOutcome::Incomplete("archive write failed — tab kept");
    }
    // 2. READ-BACK (proof, not "I wrote it") + gate on completeness. The invariant
    //    cœur: no close unless the re-read archive is non-empty and complete.
    match read_back(&record.id) {
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
// CLI (thin HTTP client).
// ---------------------------------------------------------------------------

/// The catalogue CLI: the v2 read-model (`list`) and the retire write-path (`retire`).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => run_list(),
        Some("retire") => run_retire(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  tab-atelier catalog list\n  tab-atelier catalog retire <id> --skill <name> [--prompt <text>] [--json <record>]"
            );
            2
        }
    }
}

/// `tab-atelier spawn --from-skill <name> [--task <ctx>] [--resume]`.
///
/// Resolve the folded profile by its proper NAME, build the plan (default fresh+adapt,
/// `--resume` = baseline bench), then CREATE A REAL TAB on the local daemon and launch
/// the plan's command + prompt in it.
#[must_use]
pub fn spawn_run(args: &[String]) -> i32 {
    let Some(name) = arg_after(args, "--from-skill") else {
        eprintln!("usage:\n  tab-atelier spawn --from-skill <name> [--task <ctx>] [--resume]");
        return 2;
    };
    spawn_from_skill_run(name, arg_after(args, "--task"), args.iter().any(|a| a == "--resume"))
}

/// The value after `flag` in `args` (`--from-skill <value>`).
fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `tab-atelier spawn --from-skill <name> [--task <ctx>] [--resume]`.
///
/// Resolve the folded profile by its proper NAME, build the plan, and CREATE A REAL
/// TAB (not plan-only).
///
/// NOT ported from MX: the post-spawn card seeding (`spawn-mode`/`specialty`/
/// `conventions` posted onto the new tab). Those card verbs belong to the retire
/// write-path, which this port leaves out — the routes don't exist here, so seeding
/// would just log best-effort 404s.
fn spawn_from_skill_run(name: &str, task: Option<&str>, resume: bool) -> i32 {
    let profiles = read_skill_profiles();
    let Some(profile) = resolve_skill_profile(&profiles, name) else {
        eprintln!("spawn: no skill named '{name}' (matching is by proper name, not id/slug)");
        return 1;
    };
    // `--resume` needs the baseline, which the read-model drops → read the raw records.
    let baseline_owned = resume.then(|| skill_baseline(&profile.skill)).flatten();
    let baseline = baseline_owned.as_ref().map(|(s, k)| (s.as_str(), k.as_str()));
    let plan = match plan_from_skill(profile, baseline, task, resume) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("spawn: {e}");
            return 1;
        }
    };
    match crate::cli::delegate::spawn_tab(Some(&plan.skill), None, &plan.cmd, &plan.prompt) {
        Ok(uuid) => {
            let mode = match plan.spawn_mode {
                SpawnMode::Fresh => "fresh",
                SpawnMode::Resume => "resume",
                SpawnMode::Origin => "origin",
            };
            println!(
                "{}",
                serde_json::json!({ "spawned": uuid, "skill": plan.skill, "spawnMode": mode })
            );
            0
        }
        Err(e) => {
            eprintln!("spawn: {e}");
            1
        }
    }
}

/// Parse the retire input: a full `--json <record>` body, or the `--skill <name>
/// [--prompt <text>]` shortcut for the common case.
fn parse_retire_request(args: &[String]) -> Result<RetireRequest, String> {
    if let Some(json) = arg_after(args, "--json") {
        return serde_json::from_str(json).map_err(|e| format!("bad --json record: {e}"));
    }
    let Some(skill) = arg_after(args, "--skill") else {
        return Err("need --skill <name> (or a full --json record)".into());
    };
    Ok(RetireRequest {
        stamp: V2Stamp {
            skill: Some(skill.to_string()),
            prompt: arg_after(args, "--prompt").map(str::to_string),
            ..V2Stamp::default()
        },
        ..RetireRequest::default()
    })
}

/// `tab-atelier catalog retire <id> --skill <name> [--prompt <text>] [--json <record>]`
/// `[--ack-safe-to-close]` — the v2 WRITE path.
///
/// This is the trigger of the improvement loop: a finished run becomes a catalogue
/// data-point (profile + telemetry + bilan) an eventual `spawn --from-skill` re-seeds
/// from.
///
/// `--json` carries the full [`RetireRequest`] (stamp + bilan); `--skill`/`--prompt` is
/// the shortcut. The close is destructive, so it also needs `--ack-safe-to-close` —
/// that ACK is GATE 3a of [`perform_retire`]: without it nothing is written at all and
/// the tab is kept.
#[must_use]
pub fn run_retire(args: &[String]) -> i32 {
    let Some(id) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!(
            "usage:\n  tab-atelier catalog retire <id> --skill <name> [--prompt <text>] [--json <record>] [--ack-safe-to-close]"
        );
        return 2;
    };
    let req = match parse_retire_request(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("catalog retire: {e}");
            return 2;
        }
    };
    // The skill's CURRENT folded profile is the eval's base prompt. Read BEFORE the
    // archive (the new line isn't in the file yet), so re-retiring a skill improves the
    // prompt the last record distilled.
    let profiles = read_skill_profiles();
    let base = req
        .stamp
        .skill
        .as_deref()
        .and_then(|s| resolve_skill_profile(&profiles, s))
        .cloned();
    let record = req.into_record(id, crate::unix_millis(), base.as_ref());
    let cat = catalog_path();
    // `had_session`: v2 completeness ignores it (the baseline is A/B-isolated), and this
    // verb archives no session id, so the v1 session branch is unreachable from here.
    let outcome = perform_retire(
        &record,
        args.iter().any(|a| a == "--ack-safe-to-close"),
        false,
        |c| append_catalog_line(&cat, c),
        |rid| read_back(&cat, rid),
        // v2 has no separate registration to undo: the tab lives in the daemon's
        // tabs.json, and the DELETE below persists that state itself (single writer).
        || Ok(()),
        || close_tab(id),
    );
    match outcome {
        RetireOutcome::Retired => {
            println!("{{\"retired\":\"{id}\"}}");
            0
        }
        RetireOutcome::Incomplete(flag) => {
            eprintln!("catalog retire: {flag}");
            1
        }
        RetireOutcome::CloseFailed => {
            eprintln!("catalog retire: close failed — the profile is archived, the tab is kept");
            1
        }
    }
}

/// The close seam: `DELETE /tabs/by-id/{id}` on the local daemon — the LAST effect, the
/// only one that touches a real tab.
fn close_tab(id: &str) -> std::io::Result<()> {
    let ep = crate::cli::client::discover_endpoint().map_err(std::io::Error::other)?;
    crate::cli::client::api_delete(&ep, &format!("/tabs/by-id/{id}")).map_err(std::io::Error::other)
}

/// `catalog list` — GET the read-model and print it. READ-ONLY.
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

    /// A v2 record for `skill`, hand-built so a test is readable.
    ///
    /// It carries a prompt: a v2 record with a skill but no prompt is not a profile
    /// (the eval had nothing to distill), and the read-model quarantines it — so a
    /// fixture meaning "a profile" must have one. Tests about the quarantine build
    /// their record by hand.
    fn rec(skill: &str, mode: SpawnMode, outcome: Outcome, tokens: u64) -> CatalogRecord {
        CatalogRecord {
            skill: Some(skill.to_string()),
            schema_version: Some(2),
            prompt: Some(format!("the {skill} prompt")),
            spawn_mode: Some(mode),
            outcome: Some(outcome),
            tokens: Some(tokens),
            retired_at: 1,
            ..Default::default()
        }
    }

    fn line(r: &CatalogRecord) -> String {
        serde_json::to_string(r).unwrap()
    }

    /// Write `body` to a per-call-unique temp catalogue (the suite shares one
    /// process, so a fixed name would let two tests clobber each other's file).
    fn temp_catalog(body: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ta-catalog-test-{}-{n}.jsonl", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Write records to a temp catalogue and read the folded read-model back.
    fn fold(records: &[CatalogRecord]) -> Vec<SkillProfile> {
        let body: String = records.iter().map(|r| line(r) + "\n").collect();
        let path = temp_catalog(&body);
        let out = read_skill_profiles_at(&path);
        let _ = std::fs::remove_file(&path);
        out
    }

    #[test]
    fn v1_records_are_quarantined() {
        let mut v1 = rec("legacy", SpawnMode::Fresh, Outcome::Success, 10);
        v1.schema_version = None; // a v1 record: no v2 schema
        assert!(
            fold(&[v1]).is_empty(),
            "a v1 record must not fold into the v2 read-model"
        );
    }

    #[test]
    fn a_v2_record_without_a_skill_is_quarantined() {
        // CF1: a `schema:2` record with no skill is an INCOMPLETE profile — it never
        // folds. This is why the fold key needs no slug fallback.
        let mut anonymous = rec("", SpawnMode::Fresh, Outcome::Success, 5);
        anonymous.skill = None;
        assert!(fold(&[anonymous]).is_empty(), "a skill-less v2 record must not fold");
    }

    #[test]
    fn usage_count_sums_across_instances() {
        let mut a = rec("build", SpawnMode::Fresh, Outcome::Success, 1);
        a.usage_count = Some(2);
        let mut b = rec("build", SpawnMode::Resume, Outcome::Success, 1);
        b.usage_count = Some(3);
        let profiles = fold(&[a, b]);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].usage_count, Some(5));
    }

    #[test]
    fn by_mode_partitions_the_two_arms_and_below_min_sample_concludes_nothing() {
        // 2 fresh (both success), 1 resume (problem) — under MIN_SAMPLE per arm.
        let profiles = fold(&[
            rec("build", SpawnMode::Fresh, Outcome::Success, 100),
            rec("build", SpawnMode::Fresh, Outcome::Success, 200),
            rec("build", SpawnMode::Resume, Outcome::Problem, 400),
        ]);
        let p = &profiles[0];
        assert_eq!(p.metrics.by_mode.fresh.spawns, 2);
        assert_eq!(p.metrics.by_mode.fresh.success, 2);
        assert_eq!(p.metrics.by_mode.fresh.tokens_avg, Some(150.0));
        assert_eq!(p.metrics.by_mode.resume.spawns, 1);
        assert_eq!(p.metrics.by_mode.resume.tokens_avg, Some(400.0));
        assert_eq!(p.fresh_vs_resume.verdict, AbVerdict::InsufficientSample);
        assert_eq!(p.fresh_vs_resume.fresh_n, 2);
        assert_eq!(p.fresh_vs_resume.resume_n, 1);
        // Fresh delivered 2/2, resume 0/1 → delta = +1.0 (raw, still surfaced).
        assert_eq!(p.fresh_vs_resume.delivery_delta, Some(1.0));
    }

    #[test]
    fn verdict_favours_fresh_past_the_dead_zone() {
        let mut records = Vec::new();
        for _ in 0..MIN_SAMPLE {
            records.push(rec("build", SpawnMode::Fresh, Outcome::Success, 10));
        }
        for _ in 0..MIN_SAMPLE {
            records.push(rec("build", SpawnMode::Resume, Outcome::Problem, 10));
        }
        let p = &fold(&records)[0];
        assert_eq!(p.fresh_vs_resume.verdict, AbVerdict::FreshFavored);
        assert_eq!(p.fresh_vs_resume.delivery_delta, Some(1.0));
    }

    #[test]
    fn verdict_is_inconclusive_inside_the_dead_zone() {
        // Both arms deliver 1/2 → delta 0.0, enough samples, no winner.
        let mut records = Vec::new();
        for i in 0..(MIN_SAMPLE * 2) {
            let outcome = if i % 2 == 0 { Outcome::Success } else { Outcome::Problem };
            records.push(rec("build", SpawnMode::Fresh, outcome, 10));
            records.push(rec("build", SpawnMode::Resume, outcome, 10));
        }
        let p = &fold(&records)[0];
        assert_eq!(p.fresh_vs_resume.verdict, AbVerdict::Inconclusive);
        assert_eq!(p.fresh_vs_resume.delivery_delta, Some(0.0));
    }

    #[test]
    fn origin_instances_are_excluded_from_both_arms() {
        let records = [
            rec("build", SpawnMode::Origin, Outcome::Success, 999),
            rec("build", SpawnMode::Fresh, Outcome::Success, 10),
        ];
        let p = &fold(&records)[0];
        assert_eq!(
            p.metrics.by_mode.fresh.spawns, 1,
            "the origin instance is not a fresh instance"
        );
        assert_eq!(p.metrics.by_mode.resume.spawns, 0);
        assert_eq!(p.metrics.by_mode.fresh.tokens_avg, Some(10.0));
    }

    #[test]
    fn a_tombstone_hides_the_skill_and_include_deleted_surfaces_it() {
        let mut profile = rec("build", SpawnMode::Fresh, Outcome::Success, 10);
        profile.skill = Some("build".into());
        let mut tombstone = rec("build", SpawnMode::Fresh, Outcome::Success, 0);
        tombstone.kind = RecordKind::Delete;
        let path = temp_catalog(&format!("{}\n{}\n", line(&profile), line(&tombstone)));

        let visible = read_skill_profiles_at(&path);
        let all = read_skill_profiles_all_at(&path);
        let _ = std::fs::remove_file(&path);

        assert!(visible.is_empty(), "a deleted skill is hidden by default");
        assert_eq!(all.len(), 1);
        assert!(all[0].deleted, "includeDeleted surfaces the tombstone as deleted:true");
    }

    #[test]
    fn a_restore_resurrects_a_deleted_skill() {
        let mut content = rec("build", SpawnMode::Fresh, Outcome::Success, 10);
        content.skill = Some("build".into());
        let mut tombstone = rec("build", SpawnMode::Fresh, Outcome::Success, 0);
        tombstone.kind = RecordKind::Delete;
        let mut restore = rec("build", SpawnMode::Fresh, Outcome::Success, 0);
        restore.kind = RecordKind::Restore;
        let profiles = fold(&[content, tombstone, restore]);
        assert_eq!(profiles.len(), 1, "an explicit Restore is the resurrection path");
        assert!(!profiles[0].deleted);
    }

    #[test]
    fn resume_requires_a_baseline_and_errors_without_one() {
        let profile = fold(&[rec("build", SpawnMode::Fresh, Outcome::Success, 1)])
            .into_iter()
            .next()
            .unwrap();
        let err = plan_from_skill(&profile, None, Some("t"), true).expect_err("no baseline ⇒ --resume errors");
        assert!(err.contains("no baseline"), "got: {err}");
    }

    #[test]
    fn fresh_plan_overlays_the_task_on_the_distilled_prompt() {
        let mut r = rec("build", SpawnMode::Fresh, Outcome::Success, 1);
        r.prompt = Some("You are a builder.".into());
        r.specialty = Some("rust".into());
        let profile = fold(&[r]).into_iter().next().unwrap();
        let plan = plan_from_skill(&profile, None, Some("port the catalogue"), false).unwrap();
        assert_eq!(plan.spawn_mode, SpawnMode::Fresh);
        assert_eq!(plan.cmd, FRESH_LAUNCHER);
        assert_eq!(plan.prompt, "You are a builder.\n\nTask: port the catalogue");
        assert_eq!(plan.specialty.as_deref(), Some("rust"));
    }

    #[test]
    fn resume_plan_rebuilds_the_baseline_command_and_sends_only_the_task() {
        let mut r = rec("build", SpawnMode::Fresh, Outcome::Success, 1);
        r.prompt = Some("You are a builder.".into());
        r.session_id = Some("sess-abc".into());
        r.agent_kind = Some("claude".into());
        let path = temp_catalog(&format!("{}\n", line(&r)));
        let records = read_catalog_records_at(&path);
        let _ = std::fs::remove_file(&path);

        let profile = fold(std::slice::from_ref(&r)).into_iter().next().unwrap();
        let (sid, kind) = resolve_skill_baseline(&records, "build").expect("baseline from the session record");
        assert_eq!((sid.as_str(), kind.as_str()), ("sess-abc", "claude"));
        let plan = plan_from_skill(&profile, Some((&sid, &kind)), Some("t"), true).unwrap();
        assert_eq!(plan.spawn_mode, SpawnMode::Resume);
        assert_eq!(
            plan.prompt, "Task: t",
            "the resumed session already carries the profile context"
        );
        assert!(
            plan.cmd.contains("sess-abc"),
            "the resume command names the baseline session: {}",
            plan.cmd
        );
    }

    #[test]
    fn eval_adopts_a_unanimous_clean_improvement() {
        let input = EvalInput {
            base_prompt: "Do the work.".into(),
            bilan: Bilan {
                add_directives: vec!["Always run the linter.".into()],
                ..Bilan::default()
            },
            task_literals: vec!["catalog.jsonl".into()],
            votes: EvalVotes {
                agent: EvalVote {
                    approve_prompt: true,
                    run_ok: true,
                },
                orchestrator: EvalVote {
                    approve_prompt: true,
                    run_ok: true,
                },
                olympe: EvalVote {
                    approve_prompt: true,
                    run_ok: false,
                },
            },
        };
        let result = evaluate(&input);
        assert_eq!(result.report.decision, EvalDecision::Improved);
        assert_eq!(result.report.outcome, Outcome::Success, "2/3 run_ok ⇒ derived success");
        assert!(result.resulting_prompt.contains("Always run the linter."));
    }

    #[test]
    fn eval_vetoes_an_over_fitted_directive() {
        let input = EvalInput {
            base_prompt: "Do the work.".into(),
            bilan: Bilan {
                add_directives: vec!["Fix the bug in catalog.jsonl.".into()],
                ..Bilan::default()
            },
            task_literals: vec!["catalog.jsonl".into()],
            votes: EvalVotes {
                agent: EvalVote {
                    approve_prompt: true,
                    run_ok: true,
                },
                orchestrator: EvalVote {
                    approve_prompt: true,
                    run_ok: true,
                },
                olympe: EvalVote {
                    approve_prompt: true,
                    run_ok: true,
                },
            },
        };
        let result = evaluate(&input);
        // Unanimous, but the directive leaks a task literal ⇒ statu quo (the veto wins).
        assert_eq!(result.report.decision, EvalDecision::StatuQuo);
        assert_eq!(result.report.leaked_literals, vec!["catalog.jsonl".to_string()]);
        assert_eq!(result.resulting_prompt, "Do the work.");
        assert_eq!(result.report.outcome, Outcome::Success);
    }

    #[test]
    fn eval_silence_never_improves() {
        let input = EvalInput {
            base_prompt: "Do the work.".into(),
            bilan: Bilan::default(),
            task_literals: Vec::new(),
            votes: EvalVotes::default(),
        };
        let result = evaluate(&input);
        assert_eq!(result.report.decision, EvalDecision::StatuQuo);
        assert_eq!(
            result.report.outcome,
            Outcome::Problem,
            "no run_ok vote ⇒ problem, not success"
        );
    }

    #[test]
    fn a_bilan_digests_to_one_line() {
        let bilan = Bilan {
            learned: vec!["split the diff".into()],
            problems: vec!["stale main".into()],
            add_directives: vec!["diff origin/main".into()],
            drop_directives: Vec::new(),
        };
        assert!(!bilan.is_empty());
        assert_eq!(
            bilan.one_line(),
            "learned: split the diff · problems: stale main · +prompt: diff origin/main"
        );
        assert!(Bilan::default().is_empty());
    }

    #[test]
    fn v2_stamp_opts_in_only_by_naming_a_skill() {
        assert!(!V2Stamp::default().is_v2(), "a stamp with no skill stays a v1 retire");
        let stamp = V2Stamp {
            skill: Some("build".into()),
            ..V2Stamp::default()
        };
        assert!(stamp.is_v2());
    }

    /// `arg_after` reads the flag's value, empty when the flag is absent.
    #[test]
    fn arg_after_reads_the_flag_value() {
        let args: Vec<String> = ["--from-skill", "build", "--task", "x"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(arg_after(&args, "--from-skill"), Some("build"));
        assert_eq!(arg_after(&args, "--task"), Some("x"));
        assert_eq!(arg_after(&args, "--missing"), None);
    }

    // -----------------------------------------------------------------------
    // The retire WRITE path — the loop that improves the prompt.
    // -----------------------------------------------------------------------

    const RETIRED_AT: u64 = 1_700_000_000_000;

    /// A minimal v2 request: the profile a run distils into.
    fn v2_request(skill: &str, prompt: &str) -> RetireRequest {
        RetireRequest {
            stamp: V2Stamp {
                skill: Some(skill.into()),
                prompt: Some(prompt.into()),
                ..V2Stamp::default()
            },
            ..RetireRequest::default()
        }
    }

    /// Recording seams for the retire window: `log` is the call ORDER, `archived` what
    /// the write seam received. Lets the pure retire be tested without a real tab.
    #[derive(Default)]
    struct Seams {
        log: Vec<&'static str>,
        archived: Option<CatalogRecord>,
    }

    /// Run a retire with recording seams and an INJECTED read-back (the fake archive
    /// reader), returning the outcome + the recorded call order.
    fn retire_with(
        record: &CatalogRecord,
        ack: bool,
        readback: impl FnOnce() -> Option<CatalogRecord>,
    ) -> (RetireOutcome, Vec<&'static str>) {
        use std::cell::RefCell;
        let seams = RefCell::new(Seams::default());
        let out = perform_retire(
            record,
            ack,
            false,
            |c| {
                let mut s = seams.borrow_mut();
                s.log.push("write");
                s.archived = Some(c.clone());
                Ok(())
            },
            |_id| {
                seams.borrow_mut().log.push("read-back");
                readback()
            },
            || {
                seams.borrow_mut().log.push("deregister");
                Ok(())
            },
            || {
                seams.borrow_mut().log.push("shutdown");
                Ok(())
            },
        );
        let log = seams.borrow().log.clone();
        (out, log)
    }

    // (a) round-trip byte-complete: every durable field of the retired profile — the
    // stamp AND the bilan — survives the real append + read-back path.
    #[test]
    fn retire_round_trip_is_byte_complete() {
        let bilan = Bilan {
            learned: vec!["the gate must precede the close".into()],
            problems: vec!["stale main".into()],
            add_directives: vec!["read back what you wrote".into()],
            drop_directives: vec!["drop the slug fallback".into()],
        };
        let stamp = V2Stamp {
            skill: Some("olympe".into()),
            prompt_version: Some(4),
            prompt: Some("You are Olympe.".into()),
            tools: vec!["Read".into(), "Bash".into()],
            patterns: vec!["verify-before-claim".into()],
            spawn_mode: Some(SpawnMode::Resume),
            outcome: Some(Outcome::Success),
            tokens: Some(12_345),
            cost: Some(0.42),
            difficulty: Some(3),
        };
        let record = RetireRequest {
            bilan: Some(bilan.clone()),
            stamp,
            ..RetireRequest::default()
        }
        .into_record("tab-max", RETIRED_AT, None);

        let path = temp_catalog("");
        append_catalog_line(&path, &record).expect("archive");
        let back = read_back(&path, "tab-max").expect("read-back non-empty");
        let _ = std::fs::remove_file(&path);

        assert_eq!(back, record, "every durable field round-trips byte-complete");
        assert_eq!(back.id, "tab-max");
        assert_eq!(back.skill.as_deref(), Some("olympe"));
        assert_eq!(back.prompt.as_deref(), Some("You are Olympe."));
        assert_eq!(back.prompt_version, Some(4));
        assert_eq!(back.tools, vec!["Read", "Bash"]);
        assert_eq!(back.patterns, vec!["verify-before-claim"]);
        assert_eq!(back.spawn_mode, Some(SpawnMode::Resume));
        assert_eq!(back.outcome, Some(Outcome::Success));
        assert_eq!(back.tokens, Some(12_345));
        assert_eq!(back.cost, Some(0.42));
        assert_eq!(back.difficulty, Some(3));
        assert_eq!(back.schema_version, Some(2), "a retire writes a v2 record");
        assert_eq!(back.bilan, Some(bilan), "all 4 bilan fields survive");
        assert_eq!(back.retired_at, RETIRED_AT);
    }

    // (b) THE GATE: an empty OR incomplete read-back → Incomplete, and the tab is NOT
    // closed. The assertion that counts is that the `shutdown` seam never ran.
    #[test]
    fn retire_gate_keeps_the_tab_on_empty_or_incomplete_read_back() {
        let record = v2_request("olympe", "You are Olympe.").into_record("t1", RETIRED_AT, None);

        // (i) the write "succeeded" but nothing landed → empty read-back.
        let (out, log) = retire_with(&record, true, || None);
        assert!(
            matches!(out, RetireOutcome::Incomplete(_)),
            "empty read-back → Incomplete"
        );
        assert_eq!(
            log,
            vec!["write", "read-back"],
            "no de-register, NO close — the tab is KEPT"
        );

        // (ii) the archive landed but WITHOUT the skill — it is not a v2 profile at
        // all, so the v1 bar applies and the record is incomplete.
        let mut truncated = record.clone();
        truncated.skill = None;
        let (out, log) = retire_with(&record, true, move || Some(truncated));
        assert!(
            matches!(out, RetireOutcome::Incomplete(_)),
            "incomplete read-back → Incomplete"
        );
        assert!(
            !log.contains(&"shutdown"),
            "shutdown is NEVER called on an incomplete read-back"
        );
        assert_eq!(log, vec!["write", "read-back"], "the tab is KEPT");
    }

    /// A v2 record that landed WITHOUT a prompt is CLOSABLE, not incomplete.
    ///
    /// The eval can legitimately produce no prompt — every directive vetoed on a
    /// skill's first retire, with no `--prompt` given — and there is then nothing to
    /// distill rather than something missing. Refusing the close would hold a tab
    /// forever over a profile that cannot exist. The record is archived (its telemetry
    /// is kept) and the read-model quarantines it instead.
    #[test]
    fn a_promptless_v2_retire_closes_and_is_quarantined_not_kept() {
        let record = v2_request("olympe", "You are Olympe.").into_record("t1", RETIRED_AT, None);
        let mut promptless = record.clone();
        promptless.prompt = None;

        let (out, log) = retire_with(&record, true, move || Some(promptless));
        assert_eq!(
            out,
            RetireOutcome::Retired,
            "a promptless archive is complete enough to close behind"
        );
        assert_eq!(log, vec!["write", "read-back", "deregister", "shutdown"]);

        // …and it does NOT become a profile: quarantined by the read-model, so it
        // cannot surface as one, nor blank the prompt of an existing profile.
        let mut promptless = record.clone();
        promptless.prompt = None;
        let path = temp_catalog(&format!("{}\n", line(&promptless)));
        assert!(
            read_skill_profiles_at(&path).is_empty(),
            "a v2 record without a prompt must not fold into a profile"
        );
    }

    // (d) the ORDER of effects: shutdown (the only real-tab-touching seam) runs LAST.
    #[test]
    fn retire_orders_write_readback_deregister_then_close() {
        let record = v2_request("olympe", "p").into_record("t1", RETIRED_AT, None);
        let archived = record.clone();
        let (out, log) = retire_with(&record, true, move || Some(archived));
        assert_eq!(out, RetireOutcome::Retired);
        assert_eq!(
            log,
            vec!["write", "read-back", "deregister", "shutdown"],
            "close runs LAST, only after write → read-back → de-register"
        );
    }

    // GATE 3a: no safe-to-close ACK → NO close, and nothing is even written.
    #[test]
    fn retire_without_the_safe_to_close_ack_touches_nothing() {
        let record = v2_request("olympe", "p").into_record("t1", RETIRED_AT, None);
        let archived = record.clone();
        let (out, log) = retire_with(&record, false, move || Some(archived));
        assert!(matches!(out, RetireOutcome::Incomplete(_)));
        assert!(log.is_empty(), "no ACK → not even the archive write");
    }

    // (c) a v2 retire is VISIBLE in the read-model: `schema_version == 2` + skill +
    // prompt ⇒ the record folds into a `SkillProfile`.
    #[test]
    fn a_v2_retire_lands_in_the_skill_read_model() {
        let path = temp_catalog("");
        let record = v2_request("olympe", "You are Olympe.").into_record("tab-1", RETIRED_AT, None);
        append_catalog_line(&path, &record).expect("archive");
        let back = read_back(&path, "tab-1").expect("read-back");
        assert!(back.is_complete(false), "skill + prompt ⇒ a complete v2 profile");

        let profiles = read_skill_profiles_at(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(profiles.len(), 1, "the retired profile folds into the read-model");
        assert_eq!(profiles[0].skill, "olympe");
        assert_eq!(profiles[0].prompt.as_deref(), Some("You are Olympe."));
        assert_eq!(
            profiles[0].metrics.by_mode.resume.spawns + profiles[0].metrics.by_mode.fresh.spawns,
            0
        );
    }

    // A retire that names NO skill is an incomplete profile: the gate refuses the close
    // and the record is quarantined from the read-model (no half-built profile).
    #[test]
    fn a_skill_less_retire_is_quarantined() {
        let record = RetireRequest::default().into_record("t1", RETIRED_AT, None);
        assert_eq!(record.schema_version, Some(2));
        assert!(
            !record.is_complete(false),
            "no skill/prompt ⇒ the gate refuses the close"
        );

        let path = temp_catalog("");
        append_catalog_line(&path, &record).expect("archive");
        let profiles = read_skill_profiles_at(&path);
        let _ = std::fs::remove_file(&path);
        assert!(profiles.is_empty(), "an incomplete profile is quarantined");

        let archived = record.clone();
        let (out, log) = retire_with(&record, true, move || Some(archived));
        assert!(matches!(out, RetireOutcome::Incomplete(_)));
        assert!(!log.contains(&"shutdown"), "no close on an incomplete profile");
    }

    // An EMPTY bilan records nothing — a telemetry-only retire stays untouched.
    #[test]
    fn an_empty_bilan_records_nothing() {
        let record = RetireRequest {
            bilan: Some(Bilan::default()),
            stamp: v2_request("olympe", "p").stamp,
            ..RetireRequest::default()
        }
        .into_record("t1", RETIRED_AT, None);
        assert!(record.bilan.is_none());
    }

    // The CLI input contract: a retire body carries the camelCase stamp (flattened at
    // the top level) + the nested bilan, as the orchestrator sends it.
    #[test]
    fn a_retire_body_parses_the_stamp_and_the_bilan() {
        let body = r#"{"skill":"olympe","promptVersion":3,"prompt":"p","spawnMode":"fresh",
                       "outcome":"success","tokens":7,"difficulty":2,"bilan":{"learned":["x"]}}"#;
        let req: RetireRequest = serde_json::from_str(body).expect("a retire body parses");
        assert!(req.stamp.is_v2());
        assert_eq!(req.stamp.prompt_version, Some(3));
        assert_eq!(req.stamp.spawn_mode, Some(SpawnMode::Fresh));
        assert_eq!(req.stamp.outcome, Some(Outcome::Success));
        assert_eq!(req.stamp.difficulty, Some(2));
        assert_eq!(req.bilan.map(|b| b.learned.len()), Some(1));
    }

    // The verbose path (the anti-typo guard): without `--ack-safe-to-close` the verb
    // reports the refusal and writes nothing.
    #[test]
    fn the_verbose_retire_refuses_without_the_dry_run_flag() {
        let args: Vec<String> = ["t1", "--skill", "olympe", "--prompt", "p"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(!args.iter().any(|a| a == "--ack-safe-to-close"));
        assert_eq!(run_retire(&args), 1, "no ACK ⇒ refusal, exit 1");
    }

    // -----------------------------------------------------------------------
    // The éval-à-3 WIRED INTO the retire path — the loop actually IMPROVES the prompt
    // it archives, instead of merely recording telemetry + a hand-written --prompt.
    // -----------------------------------------------------------------------

    /// The base profile the eval starts from: a skill's previously-archived prompt.
    fn base_profile(prompt: &str, version: Option<u32>) -> SkillProfile {
        SkillProfile {
            skill: "olympe".into(),
            prompt: Some(prompt.into()),
            prompt_version: version,
            ..SkillProfile::default()
        }
    }

    /// Three unanimous approving votes — the consensus the eval needs to improve.
    fn unanimous() -> EvalVotes {
        let yes = EvalVote {
            approve_prompt: true,
            run_ok: true,
        };
        EvalVotes {
            agent: yes,
            orchestrator: yes,
            olympe: yes,
        }
    }

    /// A v2 request carrying a bilan + the éval inputs (votes, task literals).
    fn eval_request(bilan: Bilan, votes: EvalVotes, task_literals: Vec<String>) -> RetireRequest {
        RetireRequest {
            bilan: Some(bilan),
            votes,
            task_literals,
            stamp: V2Stamp {
                skill: Some("olympe".into()),
                ..V2Stamp::default()
            },
        }
    }

    // (a) THE LOOP: a clean, generalisable bilan + a unanimous éval-à-3 ⇒ the archived
    // record carries the DERIVED prompt (`apply_directives` ran), a bumped
    // `prompt_version`, and the archived eval report.
    #[test]
    fn a_clean_bilan_derives_the_improved_prompt() {
        let req = eval_request(
            Bilan {
                add_directives: vec!["Always run the linter.".into()],
                ..Bilan::default()
            },
            unanimous(),
            vec!["catalog.jsonl".into()],
        );
        let base = base_profile("You are Olympe.", Some(4));
        let record = req.into_record("t1", RETIRED_AT, Some(&base));

        let prompt = record.prompt.as_deref().expect("a derived prompt");
        assert!(prompt.contains("You are Olympe."), "the base is kept, then extended");
        assert!(prompt.contains("Always run the linter."), "apply_directives ran");
        assert_eq!(record.prompt_version, Some(5), "previous profile version + 1");
        assert_eq!(record.bilan.as_ref().map(|b| b.add_directives.len()), Some(1));
        assert!(record.is_complete(false), "a derived prompt passes the v2 gate");
        let eval = record.eval.expect("the eval report is archived");
        assert_eq!(eval.decision, EvalDecision::Improved);
        assert_eq!(eval.outcome, Outcome::Success);
    }

    // (b) THE VETO (anti-over-fit): a directive that leaks a task literal ⇒ statu quo,
    // and the ORIGINAL prompt is archived — the over-fitted directive never pollutes the
    // profile.
    #[test]
    fn a_leaky_directive_is_vetoed_and_the_original_prompt_stands() {
        let req = eval_request(
            Bilan {
                add_directives: vec!["Fix the bug in catalog.jsonl.".into()],
                ..Bilan::default()
            },
            unanimous(),
            vec!["catalog.jsonl".into()],
        );
        let base = base_profile("You are Olympe.", Some(4));
        let record = req.into_record("t1", RETIRED_AT, Some(&base));

        assert_eq!(
            record.prompt.as_deref(),
            Some("You are Olympe."),
            "the original prompt stands"
        );
        assert!(
            !record.prompt.as_deref().unwrap().contains("catalog.jsonl"),
            "the leaked literal never reaches the profile"
        );
        assert_eq!(record.prompt_version, Some(4), "no bump on a statu quo");
        let eval = record.eval.expect("the veto is traced");
        assert_eq!(eval.decision, EvalDecision::StatuQuo);
        assert_eq!(eval.leaked_literals, vec!["catalog.jsonl".to_string()]);
    }

    // (c) DISSENT: 2/3 approvals ⇒ statu quo, the original prompt kept.
    #[test]
    fn a_dissent_keeps_the_original_prompt() {
        let mut votes = unanimous();
        votes.olympe.approve_prompt = false;
        let req = eval_request(
            Bilan {
                add_directives: vec!["Always run the linter.".into()],
                ..Bilan::default()
            },
            votes,
            Vec::new(),
        );
        let base = base_profile("You are Olympe.", Some(2));
        let record = req.into_record("t1", RETIRED_AT, Some(&base));

        assert_eq!(record.prompt.as_deref(), Some("You are Olympe."));
        assert_eq!(record.eval.map(|e| e.decision), Some(EvalDecision::StatuQuo));
    }

    // (d) THE MANUAL OVERRIDE WINS: an explicit --prompt is never overwritten by the
    // derivation, even with a clean bilan + a unanimous éval.
    #[test]
    fn an_explicit_prompt_is_never_overwritten() {
        let req = RetireRequest {
            stamp: V2Stamp {
                skill: Some("olympe".into()),
                prompt: Some("Hand-written.".into()),
                ..V2Stamp::default()
            },
            ..eval_request(
                Bilan {
                    add_directives: vec!["Always run the linter.".into()],
                    ..Bilan::default()
                },
                unanimous(),
                Vec::new(),
            )
        };
        let base = base_profile("You are Olympe.", Some(4));
        let record = req.into_record("t1", RETIRED_AT, Some(&base));

        assert_eq!(record.prompt.as_deref(), Some("Hand-written."));
        assert!(record.eval.is_none(), "no eval ran under a manual prompt");
    }

    // (e) DEGRADED: a retire with no bilan is telemetry-only — no eval runs, the stamp's
    // prompt stands, and the current behaviour is preserved.
    #[test]
    fn a_retire_without_a_bilan_runs_no_eval() {
        let record = v2_request("olympe", "You are Olympe.").into_record("t1", RETIRED_AT, None);
        assert!(record.eval.is_none(), "no bilan ⇒ no eval");
        assert_eq!(record.prompt.as_deref(), Some("You are Olympe."));
        assert!(record.bilan.is_none());
        assert!(record.is_complete(false));
    }
}
