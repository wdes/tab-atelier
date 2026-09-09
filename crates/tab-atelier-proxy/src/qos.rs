// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! Sharing one upstream quota between several people.
//!
//! Everyone behind this proxy spends the *same* Claude subscription, so the
//! scarce resource is Anthropic's rate limit, not anything on this machine.
//! That makes this an allocation problem rather than a protection one, and it
//! rules out the obvious answer: a fixed cap per person is not
//! work-conserving. Five accounts with a 20% cap each, four of them idle, and
//! the fifth is throttled while 80% of the quota goes unused.
//!
//! So: **weighted fair share**, in the fair-queueing tradition (Demers,
//! Keshav & Shenker, SIGCOMM '89; Shreedhar & Varghese's DRR, SIGCOMM '95).
//! Each account earns credit continuously at a rate proportional to its
//! weight, and a call goes when its account has credit to cover it.
//!
//! The work-conserving part is the denominator: credit is divided among the
//! accounts that are ACTIVE, so an idle one contributes nothing to it and its
//! share is split between the rest. Nobody is capped at their nominal
//! fraction while capacity sits unused.
//!
//! Credit accrues against wall-clock time rather than a round counter, which
//! is what makes the shares hold under real scarcity: when the binding
//! constraint is the upstream budget, whoever has credit proceeds and the
//! others wait, in proportion. Gating on the global budget FIRST — the obvious
//! implementation — makes everyone wait equally and the weights never apply at
//! all.
//!
//! # Three things this gets right that a naive limiter does not
//!
//! **The unit is tokens, not requests.** A 200k-context call costs a thousand
//! times what a small one does. Counting requests lets one person with huge
//! prompts starve everybody while looking like a light user.
//!
//! **Capacity is measured, not invented.** Anthropic reports
//! `anthropic-ratelimit-tokens-remaining` and friends on every response. The
//! proxy reads them ([`Sched::observe`]) instead of guessing a number, and
//! until it has seen them it does not throttle at all — inventing scarcity
//! that upstream is not applying would be worse than doing nothing.
//!
//! **The true cost arrives late.** A call's real token count is only known
//! once the response completes, so admission charges an estimate and
//! [`Sched::settle`] reconciles the difference into the account's deficit. An
//! underestimate is paid back out of the next turn rather than forgiven.
//!
//! # What this is not
//!
//! It does not smooth *upstream's* limit — if Anthropic 429s, everyone is
//! already stuck. It reacts to that ([`Sched::on_429`]) by backing off and
//! recovering gradually, so the fleet stops hammering a limit it has hit.

// Token counts and millisecond timestamps are both far below 2^53, where f64
// is exact, so the precision/truncation lints are false positives throughout
// this module. Scoped here rather than crate-wide, so they keep firing
// anywhere the values are not known to be small.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::time::Duration;

/// How long a call may wait for its turn before being turned away.
///
/// Past this, a 429 with an honest `Retry-After` beats a connection held open
/// — the client is going to retry anyway, and it can do something else
/// meanwhile.
pub const MAX_WAIT: Duration = Duration::from_secs(5);

/// How much unspent share an account may bank, in seconds of its own rate.
///
/// Credit is a turn-taking device, not a savings account: without a cap,
/// someone idle all morning returns and empties the window in one burst.
const BURST_SECS: f64 = 10.0;

/// Reserve kept free of the upstream window.
///
/// Draining Anthropic's budget to exactly zero earns a 429 for whoever asks
/// next, which is worse than making them wait a moment.
const HEADROOM: u64 = 2_000;

/// When an idle account leaves the round.
///
/// Nothing in flight and no traffic for this long: its share is redistributed
/// rather than reserved for it.
const IDLE_AFTER_MS: u64 = 60_000;

/// What the scheduler decided about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Go now. `charged` is the estimate debited; hand it back to
    /// [`Sched::settle`] with the real figure when the response finishes.
    Go { charged: u64 },
    /// Not yet — ask again after this long.
    Wait(Duration),
    /// Give up and tell the client when to come back, in seconds.
    Reject { retry_after: u64 },
}

#[derive(Debug, Default, Clone)]
struct Acct {
    weight: u32,
    /// Tokens of share in hand. Negative means the account overdrew on a
    /// previous call (an underestimate) and owes the difference.
    credit: f64,
    inflight: u64,
    last_seen_ms: u64,
}

impl Acct {
    const fn active(&self, now_ms: u64) -> bool {
        self.inflight > 0 || now_ms.saturating_sub(self.last_seen_ms) < IDLE_AFTER_MS
    }
}

/// The scheduler.
///
/// Deliberately a pure state machine: every method takes `now_ms` rather than
/// reading the clock, so tests drive time directly instead of sleeping and
/// hoping. The async waiting lives in the caller.
#[derive(Debug)]
pub struct Sched {
    accounts: BTreeMap<String, Acct>,
    /// Tokens left in the current upstream window, as last reported. `None`
    /// until upstream has told us — and while it is `None`, nothing is
    /// throttled.
    budget: Option<u64>,
    /// When the upstream window refills, in unix milliseconds.
    window_reset_ms: u64,
    /// Refuse everything until this instant, after a 429.
    backoff_until_ms: u64,
    /// Last time share was handed out.
    last_accrual_ms: u64,
    admitted: u64,
    rejected: u64,
}

impl Default for Sched {
    fn default() -> Self {
        Self::new()
    }
}

impl Sched {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            accounts: BTreeMap::new(),
            budget: None,
            window_reset_ms: 0,
            backoff_until_ms: 0,
            last_accrual_ms: 0,
            admitted: 0,
            rejected: 0,
        }
    }

    /// Tokens per second the whole proxy may spend, from what upstream last
    /// reported. `None` means no signal yet, which means no throttling.
    fn rate(&self, now_ms: u64) -> Option<f64> {
        let budget = self.budget?;
        let left_ms = self.window_reset_ms.saturating_sub(now_ms).max(1);
        // Spread the remaining budget evenly over the rest of the window. A
        // proxy that spent it as fast as it arrived would earn a 429 halfway
        // through and stall everyone for the remainder.
        Some((budget as f64 * 1000.0) / left_ms as f64)
    }

    /// Hand every active account its weighted slice of elapsed capacity.
    ///
    /// This is where work conservation lives: the denominator is the weight of
    /// the accounts that are ACTIVE, so an idle account contributes nothing to
    /// it and its share is divided among the rest.
    fn accrue(&mut self, now_ms: u64) {
        let Some(rate) = self.rate(now_ms) else {
            self.last_accrual_ms = now_ms;
            return;
        };
        let dt = now_ms.saturating_sub(self.last_accrual_ms) as f64 / 1000.0;
        self.last_accrual_ms = now_ms;
        if dt <= 0.0 {
            return;
        }
        let total: u32 = self
            .accounts
            .values()
            .filter(|a| a.active(now_ms))
            .map(|a| a.weight.max(1))
            .sum();
        if total == 0 {
            return;
        }
        for a in self.accounts.values_mut() {
            if !a.active(now_ms) {
                // Out of the round: no share, and no banking of what it is not
                // taking.
                a.credit = a.credit.min(0.0);
                continue;
            }
            let share = f64::from(a.weight.max(1)) / f64::from(total);
            let earned = rate * share * dt;
            let ceiling = rate * share * BURST_SECS;
            a.credit = (a.credit + earned).min(ceiling.max(1.0));
        }
    }

    /// Ask whether a call may go now.
    ///
    /// `est` is the estimated token cost ([`estimate_cost`]); `waited` is how
    /// long this call has already been queued, which is what turns a `Wait`
    /// into a `Reject` rather than an unbounded hold.
    pub fn try_admit(&mut self, id: &str, weight: u32, est: u64, now_ms: u64, waited: Duration) -> Decision {
        let weight = weight.max(1);
        let est = est.max(1);

        // Upstream has told us to stop. Nothing clever to do: everybody waits,
        // and weight is no exemption — the limit is on the shared account.
        if now_ms < self.backoff_until_ms {
            let left_ms = self.backoff_until_ms - now_ms;
            return if waited >= MAX_WAIT {
                self.rejected += 1;
                Decision::Reject {
                    retry_after: left_ms.div_ceil(1000).max(1),
                }
            } else {
                Decision::Wait(Duration::from_millis(left_ms.min(500)))
            };
        }
        // The window rolled over, or backoff just expired: the old reading is
        // stale. Go back to "unknown", which means unthrottled until upstream
        // reports again — it is the authority, not our arithmetic.
        if self.window_reset_ms != 0 && now_ms >= self.window_reset_ms {
            self.budget = None;
            self.window_reset_ms = 0;
        }

        let entry = self.accounts.entry(id.to_owned()).or_insert_with(|| Acct {
            weight,
            credit: 0.0,
            inflight: 0,
            last_seen_ms: now_ms,
        });
        entry.weight = weight;
        entry.last_seen_ms = now_ms;

        // No capacity signal yet → do not throttle. Inventing scarcity that
        // upstream is not applying would be worse than doing nothing.
        let Some(rate) = self.rate(now_ms) else {
            self.charge(id, est, now_ms);
            self.admitted += 1;
            return Decision::Go { charged: est };
        };
        self.accrue(now_ms);

        // The window itself is nearly spent. This is the one case where nobody
        // proceeds regardless of credit: the tokens are simply not there.
        if self.budget.is_some_and(|b| b.saturating_sub(est) < HEADROOM) {
            let left = self.window_reset_ms.saturating_sub(now_ms).div_ceil(1000).max(1);
            return if waited >= MAX_WAIT {
                self.rejected += 1;
                Decision::Reject { retry_after: left }
            } else {
                Decision::Wait(Duration::from_millis(250))
            };
        }

        let credit = self.accounts.get(id).map_or(0.0, |a| a.credit);
        if credit >= est as f64 {
            self.charge(id, est, now_ms);
            self.admitted += 1;
            return Decision::Go { charged: est };
        }

        // Short of credit: say precisely how long this account's own share
        // takes to cover the shortfall, rather than polling blindly.
        let total: u32 = self
            .accounts
            .values()
            .filter(|a| a.active(now_ms))
            .map(|a| a.weight.max(1))
            .sum();
        let share = f64::from(weight) / f64::from(total.max(1));
        let per_sec = (rate * share).max(1.0);
        let secs = (est as f64 - credit) / per_sec;
        if waited >= MAX_WAIT {
            self.rejected += 1;
            Decision::Reject {
                retry_after: (secs.ceil() as u64).clamp(1, 60),
            }
        } else {
            Decision::Wait(Duration::from_millis(((secs * 1000.0) as u64).clamp(20, 500)))
        }
    }

    fn charge(&mut self, id: &str, est: u64, now_ms: u64) {
        if let Some(a) = self.accounts.get_mut(id) {
            a.credit -= est as f64;
            a.inflight += 1;
            a.last_seen_ms = now_ms;
        }
        if let Some(b) = self.budget.as_mut() {
            *b = b.saturating_sub(est);
        }
    }

    /// Reconcile a finished call: the estimate is replaced by what it really
    /// cost.
    pub fn settle(&mut self, id: &str, est: u64, actual: u64, now_ms: u64) {
        let delta = actual as f64 - est as f64;
        if let Some(a) = self.accounts.get_mut(id) {
            a.inflight = a.inflight.saturating_sub(1);
            a.last_seen_ms = now_ms;
            // Underestimated → the difference comes out of the next turn, or
            // guessing low would buy an unlimited share. Overestimated →
            // credited back, or a cautious estimator would throttle its own
            // account forever.
            a.credit -= delta;
        }
        if let Some(b) = self.budget.as_mut() {
            *b = if delta > 0.0 {
                b.saturating_sub(delta as u64)
            } else {
                b.saturating_add((-delta) as u64)
            };
        }
    }

    /// Learn the real capacity from an upstream response.
    ///
    /// `remaining` and `reset_in_secs` come from
    /// `anthropic-ratelimit-tokens-remaining` and `-reset`. Trusting
    /// upstream's own numbers means the limiter tracks the actual plan —
    /// including a change to it — without anyone editing a config.
    pub const fn observe(&mut self, remaining: Option<u64>, reset_in_secs: Option<u64>, now_ms: u64) {
        if let Some(r) = remaining {
            self.budget = Some(r);
        }
        if let Some(secs) = reset_in_secs {
            self.window_reset_ms = now_ms + secs * 1000;
        }
        if self.last_accrual_ms == 0 {
            self.last_accrual_ms = now_ms;
        }
    }

    /// Upstream said no. Stop sending until it says otherwise.
    pub fn on_429(&mut self, retry_after_secs: Option<u64>, now_ms: u64) {
        let wait = retry_after_secs.unwrap_or(5).clamp(1, 300);
        self.backoff_until_ms = now_ms + wait * 1000;
        // Whatever the last reading claimed, the window is spent — and it
        // expires when the backoff does, so traffic probes again then rather
        // than staying blocked until a stale reset time.
        self.budget = Some(0);
        self.window_reset_ms = self.backoff_until_ms;
    }

    /// Diagnostics for the dashboard.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "budget_tokens": self.budget,
            "window_resets_in": self.window_reset_ms.saturating_sub(now_ms) / 1000,
            "backoff_for": self.backoff_until_ms.saturating_sub(now_ms) / 1000,
            "rate_per_sec": self.rate(now_ms),
            "admitted": self.admitted,
            "rejected": self.rejected,
            "accounts": self.accounts.iter().map(|(id, a)| {
                serde_json::json!({
                    "id": id,
                    "weight": a.weight,
                    "credit": a.credit.round(),
                    "inflight": a.inflight,
                    "active": a.active(now_ms),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// Guess what a request will cost, before upstream has said.
///
/// Input is approximated at 4 bytes per token — the usual rule of thumb for
/// English, and close enough for a scheduling decision that gets reconciled
/// against the truth moments later. `max_tokens` is added because that is what
/// the caller has reserved the right to generate, and admitting on the
/// assumption of a short answer is how a scheduler oversubscribes.
#[must_use]
pub fn estimate_cost(body: &[u8]) -> u64 {
    let input = body.len() as u64 / 4;
    let max_tokens = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("max_tokens").and_then(serde_json::Value::as_u64))
        .unwrap_or(1024);
    (input + max_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(d: &Decision) -> bool {
        matches!(d, Decision::Go { .. })
    }

    const T0: u64 = 1_700_000_000_000;

    /// A window that is genuinely too small for everyone who wants it — the
    /// only condition under which shares mean anything.
    fn contended() -> Sched {
        let mut s = Sched::new();
        // 60k tokens over 60s = 1000 tokens/sec for everybody together.
        s.observe(Some(60_000), Some(60), T0);
        s
    }

    /// Until upstream reports a limit, the proxy must not invent one.
    #[test]
    fn nothing_is_throttled_before_upstream_says_anything() {
        let mut s = Sched::new();
        for i in 0..50 {
            let d = s.try_admit("ada", 1, 100_000, T0 + i * 10, Duration::ZERO);
            assert!(go(&d), "unthrottled proxy refused call {i}: {d:?}");
        }
    }

    /// The headline property, and the one a fixed per-person cap gets wrong:
    /// an idle account's share is lent out, not reserved.
    #[test]
    fn one_busy_account_may_use_the_whole_quota() {
        let mut s = contended();
        // Grace and Alan exist but went quiet long ago.
        s.try_admit("grace", 1, 10, T0, Duration::ZERO);
        s.try_admit("alan", 1, 10, T0, Duration::ZERO);
        let alone = T0 + 10 * 60_000; // well past IDLE_AFTER for the others

        let mut served = 0u32;
        for i in 0..30 {
            if go(&s.try_admit("ada", 1, 900, alone + i * 1000, Duration::ZERO)) {
                served += 1;
                s.settle("ada", 900, 900, alone + i * 1000);
            }
        }
        // At 1000 tokens/sec and 900 per call, one second apart, a lone
        // account should clear nearly all of them. A third of them would mean
        // it was still being held to a 1-of-3 share.
        assert!(
            served >= 25,
            "an alone account should get the whole rate, served {served}/30"
        );
    }

    /// Under real contention, weights decide the split.
    #[test]
    fn weights_decide_the_split_when_everyone_is_busy() {
        let mut s = contended();
        let (mut ada, mut grace) = (0u32, 0u32);
        // Both ask far faster than capacity allows, for a simulated minute.
        // Ada is weight 3, Grace weight 1.
        for tick in 0..600u64 {
            let now = T0 + tick * 100;
            if go(&s.try_admit("ada", 3, 500, now, Duration::ZERO)) {
                ada += 1;
                s.settle("ada", 500, 500, now);
            }
            if go(&s.try_admit("grace", 1, 500, now, Duration::ZERO)) {
                grace += 1;
                s.settle("grace", 500, 500, now);
            }
        }
        assert!(ada + grace > 20, "nothing was served at all: {ada}/{grace}");
        let ratio = f64::from(ada) / f64::from(grace.max(1));
        assert!(
            (2.0..4.5).contains(&ratio),
            "weight 3 vs 1 should serve roughly 3:1, got {ada}:{grace} ({ratio:.2})"
        );
    }

    /// Cost is tokens, so a big-context caller gets fewer turns — which is
    /// exactly what a per-REQUEST limiter would have got wrong.
    #[test]
    fn a_huge_context_costs_more_turns_than_a_small_one() {
        let mut s = contended();
        let (mut big, mut small) = (0u32, 0u32);
        for tick in 0..600u64 {
            let now = T0 + tick * 100;
            if go(&s.try_admit("big", 1, 20_000, now, Duration::ZERO)) {
                big += 1;
                s.settle("big", 20_000, 20_000, now);
            }
            if go(&s.try_admit("small", 1, 500, now, Duration::ZERO)) {
                small += 1;
                s.settle("small", 500, 500, now);
            }
        }
        assert!(
            small > big * 4,
            "equal weights spend equal TOKENS, so the small caller gets many more calls: {small} vs {big}"
        );
    }

    /// An underestimate must be repaid, or a caller who always guesses low
    /// buys itself an unlimited share.
    #[test]
    fn an_underestimate_is_paid_back_out_of_the_next_turn() {
        let mut s = contended();
        // Let some credit accrue, then spend a little of it.
        s.try_admit("ada", 1, 10, T0 + 5_000, Duration::ZERO);
        let before = s.accounts["ada"].credit;
        s.settle("ada", 1_000, 50_000, T0 + 5_000);
        let after = s.accounts["ada"].credit;
        assert!(
            (before - after - 49_000.0).abs() < 1.0,
            "the 49k difference must come out of credit: {before} → {after}"
        );
        // And an overestimate is credited back, or a cautious estimator would
        // throttle its own account forever.
        s.settle("ada", 50_000, 1_000, T0 + 5_000);
        assert!((s.accounts["ada"].credit - before).abs() < 1.0);
    }

    /// Waiting is bounded: past [`MAX_WAIT`] the answer is a 429 with a figure the
    /// client can act on, not a longer hold.
    #[test]
    fn a_call_that_waits_too_long_is_rejected_with_a_retry_after() {
        let mut s = contended();
        // Far more than this account can earn in the wait window.
        let huge = 50_000;
        assert!(matches!(
            s.try_admit("ada", 1, huge, T0 + 100, Duration::ZERO),
            Decision::Wait(_)
        ));
        match s.try_admit("ada", 1, huge, T0 + 100, MAX_WAIT) {
            Decision::Reject { retry_after } => {
                assert!(
                    (1..=60).contains(&retry_after),
                    "retry_after out of range: {retry_after}"
                );
            }
            other => panic!("expected a rejection once MAX_WAIT is exceeded, got {other:?}"),
        }
    }

    /// When upstream 429s the fleet must stop — continuing to send is what
    /// turns one 429 into a sustained outage for everybody.
    #[test]
    fn an_upstream_429_stops_everyone_until_it_expires() {
        let mut s = contended();
        s.on_429(Some(30), T0);
        assert!(!go(&s.try_admit("ada", 1, 10, T0 + 5_000, Duration::ZERO)));
        assert!(
            !go(&s.try_admit("grace", 9, 10, T0 + 5_000, Duration::ZERO)),
            "a heavy weight is no exemption from the shared account's limit"
        );
        match s.try_admit("ada", 1, 10, T0 + 5_000, MAX_WAIT) {
            Decision::Reject { retry_after } => assert!((1..=30).contains(&retry_after)),
            other => panic!("expected a rejection during backoff, got {other:?}"),
        }
        // Once the backoff expires, traffic probes again rather than staying
        // blocked until some stale reset time.
        assert!(
            go(&s.try_admit("ada", 1, 10, T0 + 31_000, Duration::ZERO)),
            "backoff must expire into an unthrottled probe"
        );
    }

    #[test]
    fn a_rolled_window_restores_capacity() {
        let mut s = Sched::new();
        s.observe(Some(2_100), Some(10), T0);
        assert!(matches!(
            s.try_admit("ada", 1, 50_000, T0 + 100, Duration::ZERO),
            Decision::Wait(_)
        ));
        // Past the reset the reading is stale, so nothing is throttled until
        // upstream reports again.
        assert!(go(&s.try_admit("ada", 1, 50_000, T0 + 11_000, Duration::ZERO)));
    }

    #[test]
    fn idle_credit_does_not_accumulate_into_a_burst() {
        let mut s = contended();
        s.try_admit("ada", 1, 10, T0, Duration::ZERO);
        // Ada goes quiet for an hour while Grace works.
        for t in 0..60u64 {
            s.try_admit("grace", 1, 500, T0 + t * 60_000, Duration::ZERO);
        }
        let ada = s.accounts["ada"].credit;
        let cap = s.rate(T0).unwrap_or(1.0) * BURST_SECS;
        assert!(
            ada <= cap,
            "an idle account banked {ada} tokens of credit (cap {cap}) — it would burst on return"
        );
    }

    #[test]
    fn the_estimate_accounts_for_what_the_caller_reserved() {
        // max_tokens is part of the cost: admitting on the hope of a short
        // answer is how a scheduler oversubscribes.
        let body = br#"{"model":"m","max_tokens":8000,"messages":[{"role":"user","content":"hi"}]}"#;
        let est = estimate_cost(body);
        assert!(est > 8_000, "max_tokens must be counted: {est}");
        // Body size counts too, so a huge prompt is not free.
        let big = format!(r#"{{"max_tokens":100,"messages":"{}"}}"#, "x".repeat(400_000));
        assert!(estimate_cost(big.as_bytes()) > 100_000);
        // Junk bodies still cost something rather than nothing.
        assert!(estimate_cost(b"not json") >= 1);
    }
}
