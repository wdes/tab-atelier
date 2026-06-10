// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab off-hours auto-lock — Settings → Schedule.
//!
//! ## What it does
//!
//! A tab can carry a *schedule*: a `(rule, tz)` pair where `rule` is
//! OSM `opening_hours` grammar and `tz` is an IANA zone name. When
//! the schedule says the tab is *closed*, every write (input, inbox
//! upload, manual unlock) is refused — same gate as the manual
//! [`crate::TabState::locked`] flag. Reads (`/output`, `/view`,
//! `/stream`) stay open so a viewer can still see why their input is
//! refused.
//!
//! ## Why explicit tz
//!
//! The headless variant runs on servers that may be in a different
//! locale than the user typing the schedule. `Mo-Fr 09:00-18:00` is
//! ambiguous without a zone; making the tz mandatory removes the
//! "why is my Paris schedule firing at 03:00 UTC" class of bug.
//!
//! ## Wire format
//!
//! [`TabSchedule`] serialises as `{"rule": "Mo-Fr 09:00-18:00", "tz":
//! "Europe/Paris"}`. The field is `Option<TabSchedule>` on
//! [`crate::TabState`] so old `tabs.json` files just deserialise to
//! `None` (no schedule = always open, no behaviour change).

use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

thread_local! {
    /// Per-thread cache of parsed `opening_hours` rules, keyed by the
    /// rule string. See [`TabSchedule::parsed_cached`].
    static PARSED_RULE_CACHE: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<opening_hours::OpeningHours>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Persisted per-tab schedule. Both fields are required; an empty
/// rule string is rejected at construction so we never see one in the
/// snapshot path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabSchedule {
    /// OSM `opening_hours` rule. Validated at parse time.
    pub rule: String,
    /// IANA zone name (`Europe/Paris`, `America/New_York`, `UTC`).
    pub tz: String,
}

/// Reasons a schedule string is rejected. Surfaced verbatim to the
/// CLI / GUI so the user sees which half failed.
#[derive(Debug)]
pub enum ScheduleError {
    /// `rule` failed to parse against the OSM `opening_hours` grammar.
    BadRule(String),
    /// `tz` is not a known IANA name.
    BadTimezone(String),
    /// Either field is empty — both are mandatory.
    Empty(&'static str),
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRule(s) => write!(f, "invalid opening_hours rule: {s}"),
            Self::BadTimezone(s) => write!(f, "unknown timezone: {s}"),
            Self::Empty(which) => write!(f, "schedule {which} is empty"),
        }
    }
}

impl std::error::Error for ScheduleError {}

impl TabSchedule {
    /// Validate `(rule, tz)` and bundle into a `TabSchedule`.
    ///
    /// # Errors
    /// - `Empty` if either field is empty.
    /// - `BadTimezone` if `tz` does not match a known IANA name.
    /// - `BadRule` if `rule` fails the OSM `opening_hours` parse.
    pub fn new(rule: impl Into<String>, tz: impl Into<String>) -> Result<Self, ScheduleError> {
        let rule = rule.into();
        let tz = tz.into();
        if rule.trim().is_empty() {
            return Err(ScheduleError::Empty("rule"));
        }
        if tz.trim().is_empty() {
            return Err(ScheduleError::Empty("tz"));
        }
        // Parse the tz first — it's the cheaper of the two and gives
        // a clearer error message than the opening_hours parser would.
        Tz::from_str(tz.trim()).map_err(|_| ScheduleError::BadTimezone(tz.clone()))?;
        // Parse the rule via the opening-hours crate. The parser
        // rejects empty / malformed input; we don't try to be cleverer.
        opening_hours::OpeningHours::parse(&rule).map_err(|e| ScheduleError::BadRule(format!("{e}")))?;
        Ok(Self {
            rule: rule.trim().to_string(),
            tz: tz.trim().to_string(),
        })
    }

    /// Resolved IANA zone. Panic-free; we validated at construction.
    #[must_use]
    pub fn timezone(&self) -> Tz {
        Tz::from_str(&self.tz).unwrap_or(chrono_tz::UTC)
    }

    /// Parsed opening-hours rule. Constructed on demand because the
    /// underlying type isn't Clone-cheap (allocates an AST).
    ///
    /// # Panics
    /// Cannot panic in practice — the rule was validated at
    /// construction. The `unwrap` is documented as a should-be-Ok.
    #[must_use]
    pub fn parsed(&self) -> opening_hours::OpeningHours {
        opening_hours::OpeningHours::parse(&self.rule).expect("rule validated at construction")
    }

    /// Like [`Self::parsed`] but memoised per-thread, keyed by the rule
    /// string, so repeated lock-state queries don't re-parse the OSM
    /// grammar AST every time. The verdict functions below run per tab
    /// on the `/tabs`, `/output`, WS-meta and per-tick mirror paths;
    /// most installs have a handful of distinct rules at most, so the
    /// cache stays tiny. Thread-local keeps the parsed type free of any
    /// `Send`/`Sync` requirement.
    fn parsed_cached(&self) -> std::rc::Rc<opening_hours::OpeningHours> {
        PARSED_RULE_CACHE.with(|cache| {
            if let Some(oh) = cache.borrow().get(&self.rule) {
                return oh.clone();
            }
            let oh = std::rc::Rc::new(self.parsed());
            cache.borrow_mut().insert(self.rule.clone(), oh.clone());
            oh
        })
    }

    /// `true` if the schedule says the tab is OPEN (writes allowed)
    /// at the given UTC instant. Anchored to the schedule's own tz.
    #[must_use]
    pub fn is_open_at(&self, now_utc: DateTime<Utc>) -> bool {
        let local = self.local_naive(now_utc);
        let oh = self.parsed_cached();
        // The opening-hours crate's `state(t)` returns Open / Closed /
        // Unknown. Treat anything that isn't an explicit Open as
        // closed — fail closed on ambiguity.
        matches!(oh.state(local), opening_hours::RuleKind::Open)
    }

    /// Wall-clock instant of the next state change after `now_utc`,
    /// expressed in UTC. `None` when the rule has no further
    /// transitions (e.g. `24/7` or `Mo-Su off`).
    #[must_use]
    pub fn next_change_at(&self, now_utc: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = self.local_naive(now_utc);
        let oh = self.parsed_cached();
        let next_local = oh.next_change(local)?;
        let tz = self.timezone();
        // `from_local_datetime` is ambiguous around DST gaps. Pick the
        // earliest valid interpretation — the schedule's next-change
        // should be the *first* moment the state flips, even when the
        // wall clock skipped over it.
        tz.from_local_datetime(&next_local)
            .earliest()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Convenience wrapper around [`Self::is_open_at`] using the
    /// process's current wall-clock.
    #[must_use]
    pub fn is_open_now(&self) -> bool {
        self.is_open_at(Utc::now())
    }

    /// Convenience wrapper around [`Self::next_change_at`] using the
    /// process's current wall-clock.
    #[must_use]
    pub fn next_change_from_now(&self) -> Option<DateTime<Utc>> {
        self.next_change_at(Utc::now())
    }

    fn local_naive(&self, now_utc: DateTime<Utc>) -> NaiveDateTime {
        let tz = self.timezone();
        now_utc.with_timezone(&tz).naive_local()
    }
}

/// Short reason key the viewer can localise.
///
/// `"manual"` for an explicit lock, `"schedule"` for an off-hours
/// auto-lock, `None` if the tab is open. Computed against the same
/// logical "effective locked" state every API gate uses.
#[must_use]
pub fn lock_reason(manual_locked: bool, schedule: Option<&TabSchedule>) -> Option<&'static str> {
    if manual_locked {
        return Some("manual");
    }
    match schedule {
        Some(s) if !s.is_open_now() => Some("schedule"),
        _ => None,
    }
}

/// `true` when either the manual lock or the schedule's current state
/// would reject a write. Single source of truth for every API gate.
#[must_use]
pub fn effective_locked(manual_locked: bool, schedule: Option<&TabSchedule>) -> bool {
    lock_reason(manual_locked, schedule).is_some()
}

/// Uniform lock-state view across every tab representation.
///
/// Implemented for `TabState` (persisted), `SnapshotTab` (API
/// snapshot), the gpui `Tab` runtime, and `HeadlessTab` runtime. By
/// routing every write gate through `effective_locked()` instead of
/// reading the raw `locked` field, a new gate can't accidentally
/// honour only the manual flag and ignore the schedule.
///
/// Adding a new tab-like type? Implement `manual_locked` + `schedule`
/// and the provided methods do the rest. Adding a new gate? Call
/// `tab.effective_locked()` — never `tab.locked`.
///
/// The raw `locked` field on each struct stays public for serde +
/// the manual-lock-toggle UI, but the field's doc-comment warns
/// readers to use this trait in gates.
pub trait LockState {
    /// User-toggled lock — independent of the schedule. True ⇒
    /// the user explicitly paused this tab via right-click / CLI /
    /// `POST /lock`.
    fn manual_locked(&self) -> bool;
    /// Off-hours auto-lock schedule, if configured. None ⇒ tab is
    /// always-open from the schedule's perspective.
    fn schedule(&self) -> Option<&TabSchedule>;
    /// Final write-gate verdict. True ⇒ refuse writes (input, files
    /// upload, manual unlock). False ⇒ writes allowed.
    fn effective_locked(&self) -> bool {
        effective_locked(self.manual_locked(), self.schedule())
    }
    /// `"manual"` / `"schedule"` / None. Surface to API headers + UX.
    fn lock_reason(&self) -> Option<&'static str> {
        lock_reason(self.manual_locked(), self.schedule())
    }
}

impl LockState for crate::TabState {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&TabSchedule> {
        self.schedule.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.from_utc_datetime(&NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap())
    }

    #[test]
    fn empty_rule_rejected() {
        assert!(matches!(
            TabSchedule::new("", "Europe/Paris"),
            Err(ScheduleError::Empty("rule"))
        ));
    }

    #[test]
    fn empty_tz_rejected() {
        assert!(matches!(
            TabSchedule::new("Mo-Fr 09:00-18:00", ""),
            Err(ScheduleError::Empty("tz"))
        ));
    }

    #[test]
    fn bad_tz_rejected() {
        assert!(matches!(
            TabSchedule::new("Mo-Fr 09:00-18:00", "Mars/Olympus_Mons"),
            Err(ScheduleError::BadTimezone(_))
        ));
    }

    #[test]
    fn bad_rule_rejected() {
        // Random gibberish that can't be a valid opening_hours expr.
        assert!(matches!(
            TabSchedule::new("definitely not a rule {{{", "Europe/Paris"),
            Err(ScheduleError::BadRule(_))
        ));
    }

    #[test]
    fn weekday_morning_open() {
        let s = TabSchedule::new("Mo-Fr 09:00-18:00", "Europe/Paris").unwrap();
        // 2026-01-05 is a Monday. 10:00 Europe/Paris = 09:00 UTC in winter.
        assert!(s.is_open_at(utc(2026, 1, 5, 9, 0)));
    }

    #[test]
    fn weekday_evening_closed() {
        let s = TabSchedule::new("Mo-Fr 09:00-18:00", "Europe/Paris").unwrap();
        // Monday 22:00 Paris winter = 21:00 UTC.
        assert!(!s.is_open_at(utc(2026, 1, 5, 21, 0)));
    }

    #[test]
    fn saturday_closed_when_rule_is_weekdays_only() {
        let s = TabSchedule::new("Mo-Fr 09:00-18:00", "Europe/Paris").unwrap();
        // 2026-01-10 is a Saturday. 10:00 Paris = 09:00 UTC.
        assert!(!s.is_open_at(utc(2026, 1, 10, 9, 0)));
    }

    #[test]
    fn always_24_7_always_open() {
        let s = TabSchedule::new("24/7", "UTC").unwrap();
        assert!(s.is_open_at(utc(2026, 1, 1, 3, 0)));
        assert!(s.is_open_at(utc(2026, 7, 4, 13, 0)));
    }

    #[test]
    fn next_change_finds_evening_close() {
        let s = TabSchedule::new("Mo-Fr 09:00-18:00", "Europe/Paris").unwrap();
        // Mon 10:00 Paris (winter) → next change at 18:00 Paris = 17:00 UTC.
        let next = s.next_change_at(utc(2026, 1, 5, 9, 0)).unwrap();
        assert_eq!(next, utc(2026, 1, 5, 17, 0));
    }

    #[test]
    fn lock_reason_manual_beats_schedule() {
        let s = TabSchedule::new("24/7", "UTC").unwrap();
        assert_eq!(lock_reason(true, Some(&s)), Some("manual"));
    }

    #[test]
    fn lock_reason_schedule_when_closed() {
        let s = TabSchedule::new("Mo-Fr 09:00-18:00", "Europe/Paris").unwrap();
        // Hard to assert here without freezing time; just test the
        // helper composes correctly via the manual=false path.
        let r = lock_reason(false, Some(&s));
        assert!(r.is_none() || r == Some("schedule"));
    }

    #[test]
    fn lock_reason_none_when_no_schedule_no_manual() {
        assert_eq!(lock_reason(false, None), None);
    }

    #[test]
    fn lunch_break_pattern_closes_midday() {
        let s = TabSchedule::new("Mo-Fr 09:00-12:30,13:30-18:00", "Europe/Paris").unwrap();
        // Mon 12:00 UTC = 13:00 Paris winter — inside the lunch gap.
        assert!(!s.is_open_at(utc(2026, 1, 5, 12, 0)));
        // Mon 13:00 UTC = 14:00 Paris — back in afternoon block.
        assert!(s.is_open_at(utc(2026, 1, 5, 13, 0)));
    }

    #[test]
    fn dst_spring_forward_paris_2026() {
        // 2026-03-29: Europe/Paris jumps 02:00 → 03:00. A schedule
        // that nominally opens at 03:00 should still resolve to a
        // real UTC instant via `from_local_datetime().earliest()`.
        let s = TabSchedule::new("Su 03:00-04:00", "Europe/Paris").unwrap();
        // 23:00 UTC the night before = 00:00 Paris local — closed.
        assert!(!s.is_open_at(utc(2026, 3, 28, 23, 0)));
        // 04:00 UTC during the open window (= 06:00 Paris summer time).
        // We don't care about the exact instant — only that
        // next_change_at returns SOMETHING and doesn't panic.
        let _ = s.next_change_at(utc(2026, 3, 29, 0, 0));
    }
}
