// SPDX-License-Identifier: MPL-2.0

//! What a tab is *for*, as a typed value: `phase/role`, with an optional override.
//!
//! # Why this is a type and not three strings
//!
//! A tab's assignment is the stable answer to "what work is this agent doing", and it is the field
//! the dashboard's bands, the task-capacity gate and `dispatch --to <role>` all read. It is
//! deliberately separate from the volatile `context` (a free-text note about the current PR or
//! task): context changes every few minutes, an assignment changes when the work does.
//!
//! # The grammar
//!
//! ```text
//! assignment := [override ':'] phase ['/' role]
//! ```
//!
//! `review/pr`, `build/api`, `hotfix:build/api`, `build`. Everything is trimmed, and an empty
//! segment reads as absent rather than as an empty string — `"build/"` has a phase and no role,
//! which is what a human means by it.
//!
//! This lived on a retired branch as a function returning `(Option<String>, String, String)`. A
//! tuple of three positional strings is easy to build in the wrong order and impossible to display
//! back; the type below round-trips, so the string a daemon stores is the string a reader sees.

use std::fmt;

/// The phase a tab has no assignment for.
///
/// Named rather than scattered string literals: the dashboard groups by phase, and an agent with no
/// assignment has to land in a bucket that is obviously "unassigned" rather than in a band called
/// `""`.
pub const UNASSIGNED: &str = "unassigned";

/// What a tab is assigned to do.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Assignment {
    /// A narrowing of the phase, when the phase alone is too coarse — `hotfix` inside `build`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    /// The band: `scope`, `plan`, `build`, `review`, `verify`, `sweep`, `done`, … Free-form on
    /// purpose, because the fleet's workflow is the operator's to define and a closed enum here
    /// would need a release to add a phase.
    pub phase: String,
    /// What this tab does inside the phase — `api`, `ui`, `pr`. Matched by `dispatch --to <role>`.
    pub role: String,
}

impl Assignment {
    /// Parse `[override:]phase[/role]`.
    ///
    /// Never fails: an assignment is operator-typed free text, and the useful behaviour for
    /// something unexpected is to keep it rather than to refuse it. `"a/b/c"` keeps `b/c` as the
    /// role, because the role is the only field that can legitimately contain a slash (a reviewer
    /// of `infra/db`).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        // Control characters are removed before anything else, on every path — the CLI, the API and
        // any future writer. A phase or role is displayed on a dashboard, written to a log line and
        // printed in a terminal listing, so an ESC sequence in it would be *rendered* by whatever
        // reads it, and a newline would break the one-line contract the listing relies on.
        // Filtering here means no caller has to remember to.
        let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
        let raw = cleaned.trim();
        // The override may only appear before the first `/`, so a colon later in the string (a role
        // like `ui:css`) is not mistaken for one.
        let head_end = raw.find('/').unwrap_or(raw.len());
        let (over, rest) = raw[..head_end]
            .find(':')
            .map_or((None, raw), |colon| (Some(raw[..colon].trim()), &raw[colon + 1..]));
        let (phase, role) = rest.split_once('/').unwrap_or((rest, ""));
        let non_empty = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        };
        Self {
            over: over.and_then(non_empty),
            phase: phase.trim().to_owned(),
            role: role.trim().to_owned(),
        }
    }

    /// The band this assignment belongs to, for grouping — `unassigned` when there is no phase.
    #[must_use]
    pub fn phase_or_unassigned(&self) -> &str {
        if self.phase.is_empty() { UNASSIGNED } else { &self.phase }
    }

    /// Whether anything was assigned at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.phase.is_empty() && self.role.is_empty() && self.over.is_none()
    }
}

impl fmt::Display for Assignment {
    /// Back to the wire form, so a value that was parsed and then stored is the same text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(over) = &self.over {
            write!(f, "{over}:")?;
        }
        write!(f, "{}", self.phase)?;
        if !self.role.is_empty() {
            write!(f, "/{}", self.role)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Assignment, UNASSIGNED};

    #[test]
    fn a_full_assignment_splits_into_override_phase_and_role() {
        let a = Assignment::parse("hotfix:build/api");
        assert_eq!(a.over.as_deref(), Some("hotfix"));
        assert_eq!(a.phase, "build");
        assert_eq!(a.role, "api");
    }

    #[test]
    fn a_phase_and_role_need_no_override() {
        let a = Assignment::parse("review/pr");
        assert_eq!(a.over, None);
        assert_eq!(a.phase, "review");
        assert_eq!(a.role, "pr");
    }

    #[test]
    fn a_bare_phase_is_a_whole_assignment() {
        let a = Assignment::parse("build");
        assert_eq!(a.over, None);
        assert_eq!(a.phase, "build");
        assert_eq!(a.role, "", "no slash means no role, not a missing one");
    }

    #[test]
    fn a_trailing_slash_is_a_phase_without_a_role() {
        // What a human means by "build/": still build, just nobody named.
        let a = Assignment::parse("build/");
        assert_eq!(a.phase, "build");
        assert_eq!(a.role, "");
        assert_eq!(a.to_string(), "build", "the empty role is not written back");
    }

    #[test]
    fn a_role_may_contain_a_slash() {
        // The role is the only field that can legitimately carry one — a reviewer of `infra/db`.
        let a = Assignment::parse("review/infra/db");
        assert_eq!(a.phase, "review");
        assert_eq!(a.role, "infra/db");
    }

    #[test]
    fn a_colon_after_the_first_slash_is_part_of_the_role_not_an_override() {
        // `ui:css` is a role with a colon in it. Reading that colon as an override would file the
        // tab under a phase called `review/ui`.
        let a = Assignment::parse("review/ui:css");
        assert_eq!(a.over, None);
        assert_eq!(a.phase, "review");
        assert_eq!(a.role, "ui:css");
    }

    #[test]
    fn an_empty_override_is_dropped_rather_than_kept_as_an_empty_string() {
        // `:build/api` is a typo for `build/api`; keeping `Some("")` would make it a distinct value
        // from the assignment it clearly means.
        let a = Assignment::parse(":build/api");
        assert_eq!(a.over, None);
        assert_eq!(a.phase, "build");
        assert_eq!(a.role, "api");
    }

    #[test]
    fn surrounding_whitespace_never_reaches_a_band_name() {
        let a = Assignment::parse("  build / api  ");
        assert_eq!(a.phase, "build");
        assert_eq!(a.role, "api");
    }

    #[test]
    fn an_empty_assignment_is_recognisably_empty() {
        for raw in ["", "   ", "/"] {
            let a = Assignment::parse(raw);
            assert!(a.is_empty(), "{raw:?} should parse as empty, got {a:?}");
            assert_eq!(a.phase_or_unassigned(), UNASSIGNED, "{raw:?}");
        }
    }

    #[test]
    fn every_assignment_round_trips_through_its_own_text_form() {
        // The property that makes the type worth having over the tuple it replaced: what the daemon
        // stores and what a reader sees are the same string.
        for raw in [
            "build",
            "build/api",
            "hotfix:build/api",
            "review/infra/db",
            "review/ui:css",
            "  build / api  ",
        ] {
            let once = Assignment::parse(raw);
            let twice = Assignment::parse(&once.to_string());
            assert_eq!(once, twice, "{raw:?} -> {once} -> {twice}");
        }
    }

    #[test]
    fn control_characters_never_reach_a_band_name() {
        // A phase is displayed, logged and printed. An ESC sequence would be *rendered* by whatever
        // reads it — a terminal repaint from an operator-typed string — and a newline would break
        // the one-line listing contract. Filtering is in `parse`, so every writer gets it.
        let a = Assignment::parse("build\u{1b}[31m/api");
        assert_eq!(a.phase, "build[31m");
        assert_eq!(a.role, "api");
        assert!(!a.phase.contains('\u{1b}'), "{:?}", a.phase);

        let b = Assignment::parse("re\nview\t/pr");
        assert_eq!(b.phase, "review");
        assert_eq!(b.role, "pr");
        assert_eq!(b.to_string(), "review/pr");
    }

    #[test]
    fn a_phase_is_never_confused_with_unassigned() {
        // The dashboard groups by `phase_or_unassigned`; a phase literally named "unassigned" would
        // merge a real band with the empty one, so it is worth pinning that the empty case is the
        // only thing that reaches that name.
        assert_eq!(Assignment::parse("unassigned").phase_or_unassigned(), "unassigned");
        assert_eq!(Assignment::parse("").phase_or_unassigned(), UNASSIGNED);
        assert!(!Assignment::parse("unassigned").is_empty());
    }
}
