# Brain anti-storm — rework plan for PR #40

Work order for rewriting [PR #40](https://github.com/wdes/tab-atelier/pull/40)
(`a-biskoazh:feat/brain-robustness`, cross-repo, draft) before force-pushing our
version over it. Everything below concerns `src/cli/brain.rs` only.

The submitted version is mechanically clean — `cargo fmt --check` and
`cargo clippy --all-targets` pass, 29 brain tests green on top of `c0d61b6` —
but it deadlocks brain permanently, so it can't land as written.

## Baseline: what `main` already does

Do not re-add brakes for these; the PR's motivating scenario ("50 frozen tabs →
50 `continue`s in 1–2 min") is fork behaviour, not ours.

- `pick_round_robin` — at most **one** nudge per tick, fleet-wide.
- `DEFAULT_INTERVAL_SECS = 5` — so the existing nudge floor is already 1 per 5 s.
- `nudged_hash` — a given frozen screen is nudged once and never again until the
  output changes.
- `backoff_secs` — per-tab exponential backoff, 60 s → 900 s, on a repeated label.
- `ConnectivityProbe` — no sends at all while the box is offline.

## A. Keep — bounded, resumable scan

Take this part of the PR essentially as-is. It also fixes a real bug on `main`:
`brain.rs:456-463` uses `?` on the per-tab `/output` GET, so one tab closing
mid-tick aborts the whole tick and strands every tab after it.

- Filter Claude tabs up front (cheap, no HTTP), then walk them from a persistent
  `scan_cursor` via `scan_order(n, cursor)`, stopping once `TICK_BUDGET` is spent
  and resuming there next tick.
- `seen_ids` must cover **all** Claude tabs, not just the ones scanned this tick,
  so a budget-deferred tab keeps its watch state instead of having its freeze
  clock reset.
- A failing per-tab `/output` GET logs and `continue`s instead of aborting the
  tick, and still counts as scanned so the cursor moves past it.

Change from the PR: `TICK_BUDGET` was 6 s, which is **longer** than the 5 s poll
interval, so it can't bound a cycle at the default rate. Set it below
`DEFAULT_INTERVAL_SECS` (4 s), and note in the comment that the worst case is the
budget plus one already-in-flight GET (the `share_link` agent's 3 s timeout).

## B. Drop — the global nudge throttle

Delete `NUDGE_MIN_INTERVAL` and the normal-mode use of `nudge_ready`. At the
default 5 s interval the round-robin already spaces nudges ≥ 5 s apart, so a 2 s
fleet-wide throttle can never return false. It's new state and branches for a
condition that cannot occur. `nudge_ready` itself survives, but only as the
heartbeat gate in C.

`nudge_interval(systemic)` goes with it: with no normal-mode throttle there is no
second branch to select, so systemic mode calls
`nudge_ready(&mut last_nudge_at, now, CIRCUIT_BREAKER_COOLDOWN)` directly.

## C. Redesign — the two-level breaker

The PR's level-(b) storm detector must not ship. `brain.rs:685` stamps
`watch.last_api_error_at = Some(now)` on every tick for every scanned tab whose
scrollback tail matches a storm label, placed *before* the `should_nudge` freeze
gate. A frozen screen never changes, so the needle never leaves the trailing
4 KB window, so the stamp is refreshed faster than the 60 s window can age it out:

1. `api_error_sessions()` stays ≥ 5 forever,
2. `storm_freeze()` returns early before any nudge,
3. the tabs stay frozen → back to 1.

Brain then never nudges again, for any tab, including ones stuck on unrelated
errors. The PR's test passes only because it exercises `api_error_sessions` over
a static map and never runs the loop that re-stamps it.

Keep the *idea* worth keeping — a systemic freeze deserves a gentler cadence, and
it should be a spaced heartbeat rather than silence (total suppression makes
brain look dead and hides the recovery moment). Implement it as **one**
stateless-per-tick level instead:

- Delete `TabWatch::last_api_error_at`, `STORM_ERROR_THRESHOLD`,
  `STORM_ERROR_WINDOW`, `api_error_sessions`, `storm_freeze`. No sliding window,
  no time-stamped error state — those are what deadlock.
- Derive systemic-ness from the **current tick's `eligible` set**, which already
  means "frozen past `STABLE_SECS`, not yet nudged at this screen, past backoff":
  count the eligible tabs whose trigger label is an Anthropic-capacity error. If
  that count exceeds the threshold, the fleet is capped. When the tabs recover
  they leave `eligible`, the count drops, and normal cadence resumes on its own —
  the recovery path the sliding window was supposed to provide.
- `is_api_storm_label` keeps `anthropic-529`, `anthropic-rate-limited`,
  `anthropic-503`, `anthropic-5xx`. **Drop `api-retry-waiting`** — `"will retry
  in"` is Claude Code's healthy self-retry banner, and `brain.rs:225-236` says
  outright that brain stays out of its way. Counting it means five tabs
  recovering normally would throttle the whole fleet.
- Systemic → still send exactly one nudge, spaced by `nudge_ready` at the
  heartbeat interval (reuse `CIRCUIT_BREAKER_COOLDOWN`, 30 s), keeping
  `breaker_until` sticky so a spike doesn't flap. Non-systemic → untouched
  round-robin.

Log on **entering and leaving** systemic mode, not every tick. The PR's storm
message sits before the `eligible.is_empty()` check and prints every 5 s forever.

## Style — AGENTS.md compliance

- Strip the fork-internal jargon; none of it has a referent in this repo: `AXE A`
  / `AXE B`, `rate-limit finding #5`, `rejected (tichef)`, `the PO perceived as a
  freeze`, `(PR-worthy)` in a commit subject.
- "Minimal comments — only when the why is non-obvious." The submission is ~488
  added lines for ~120 of logic. Cut the ratio hard; the surviving comments
  should explain the deadlock we're avoiding, not narrate the change history.
- Update the `--help` text: it currently claims brain "backs off ~30s instead of
  nudging", which contradicts the heartbeat policy, and never mentions the
  breaker's second level.
- MPL header stays. `cargo fmt` + `cargo clippy --all-targets` under **both**
  feature configs before pushing.

## Tests

### Why a new seam is needed

#40 shipped a permanent deadlock with a green test suite because every new test
exercised a pure function over a hand-built `HashMap` — never the loop that
re-stamps it each tick. Any replacement that only tests pure helpers in isolation
will miss the same class of bug. `tick()` can't be tested (HTTP), so extract the
per-tab decision, and drive it over *many simulated ticks* with injected `now`:

```rust
/// Pure per-tab decision for one tick: fold this tick's output into `watch` and
/// return the trigger to nudge on, or None. All HTTP stays in `tick`.
fn evaluate_tab(
    watch: &mut TabWatch,
    output: &str,
    agent_state: Option<&str>,
    now: Instant,
) -> Option<Trigger>
```

It absorbs the hash/stability update, trigger detection, the recovery reset, the
`should_nudge` gate and the backoff gate — i.e. exactly the inline block that
hid the deadlock. One function replacing inline code, not a layer; it's the
minimum that makes the regression testable.

Second seam, for section C:

```rust
/// Systemic when more than CIRCUIT_BREAKER_THRESHOLD of THIS TICK's eligible
/// tabs are stuck on an Anthropic-capacity error. Sticky for the cooldown.
fn systemic_api_freeze(
    eligible: &[Eligible],
    breaker_until: &mut Option<Instant>,
    now: Instant,
) -> bool
```

Tests inject `now` and advance it by arithmetic. No `sleep`, no wall-clock waits.

### Headline regression — a capped fleet must not deadlock

The one test that would have caught #40. Under the submitted design the nudge
count here is exactly `0`, forever.

```rust
#[test]
fn frozen_api_fleet_still_gets_heartbeat_nudges() {
    // 8 tabs frozen on `529 Overloaded`, screens byte-identical every tick —
    // the population the breaker exists for. #40 stamped last_api_error_at on
    // every tick from the unchanged screen, so its 60s window could never
    // slide and brain went silent permanently. Systemic mode must throttle to
    // a heartbeat, never to zero.
    let t0 = Instant::now();
    let eligible: Vec<Eligible> = (0..8).map(|i| eligible_529(i)).collect();
    let (mut breaker, mut last_nudge, mut cursor) = (None, None, 0usize);
    let mut sent = 0;
    // 5 simulated minutes at the default 5s tick.
    for tick in 0..60 {
        let now = t0 + Duration::from_secs(tick * DEFAULT_INTERVAL_SECS);
        let systemic = systemic_api_freeze(&eligible, &mut breaker, now);
        assert!(systemic, "8 > threshold stays systemic while nothing recovers");
        if nudge_ready(&mut last_nudge, now, CIRCUIT_BREAKER_COOLDOWN) {
            assert!(pick_round_robin(&eligible, &mut cursor).is_some());
            sent += 1;
        }
    }
    assert!(sent > 0, "SILENT FOREVER — the #40 deadlock");
    // ~1 per cooldown over 300s, and never a burst.
    assert!((9..=11).contains(&sent), "heartbeat cadence, got {sent}");
}
```

### Breaker (section C)

- `systemic_api_freeze` threshold + stickiness — adapt the PR's
  `circuit_breaker_throttles_to_a_heartbeat_not_silence`: below and **at** the
  threshold → not systemic and no cooldown armed; above → systemic + armed;
  sticky through the cooldown even as the count drops; clears after it elapses;
  re-arms if still over.
- **Only capacity errors count.** 8 eligible tabs all on `connection-refused` →
  *not* systemic (local per-box faults aren't a fleet-wide cap). Mixed 3 API +
  5 local → not systemic. This is what the count-based `eligible.len()` breaker
  in #40 got wrong.
- **`api-retry-waiting` is excluded.** `assert!(!is_api_storm_label("api-retry-waiting"))`
  with a comment pointing at `brain.rs:225-236` — it's Claude Code's healthy
  self-retry banner, and counting it lets five *recovering* tabs throttle the
  fleet. Keep the positive assertions for `anthropic-529`,
  `anthropic-rate-limited`, `anthropic-503`, `anthropic-5xx`.
- **Recovery resumes normal cadence.** Systemic at `t0`; tabs recover so
  `eligible` empties; after `CIRCUIT_BREAKER_COOLDOWN` the next call returns
  false and the heartbeat gate is out of the path entirely. Locks the
  self-clearing property the sliding window was supposed to provide.
- **Heartbeat is spacing, never silence.** `nudge_ready` at
  `CIRCUIT_BREAKER_COOLDOWN` admits exactly one nudge per window: fires at `t0`,
  refused at `t0 + 5s`, fires again at `t0 + cooldown`. Rewrite of the PR's
  `nudge_throttle_spaces_bursts_and_never_simultaneous`, which must lose its
  `NUDGE_MIN_INTERVAL` half (section B deletes it).

### Per-tab evaluation (`evaluate_tab`)

- **Frozen screen is nudged once, then suppressed.** Same output across 60 ticks
  → `Some(trigger)` on the first tick past `STABLE_SECS`, then `None` for the
  rest via `nudged_hash`. Documents the real "eventually" semantics: recovery
  needs the *output* to change, which is why a fleet-wide time window can't be
  the release condition.
- **Moving output is never nudged.** Feed a live countdown (`"will retry in 1m
  57s"`, `"…56s"`, …) for 60 ticks → always `None`, because each change resets
  `stable_since`. The healthy-banner case end to end.
- **Recovery clears backoff state.** After a nudge, feed clean output →
  `nudge_streak == 0`, `last_label == None`, `next_nudge_at == None`.
- **Deferral doesn't reset the freeze clock.** Skip `evaluate_tab` for several
  ticks (as `TICK_BUDGET` truncation does), then call it with unchanged output:
  `stable_since` is untouched and the tab is still eligible. Guards the
  `seen_ids`-covers-all-Claude-tabs invariant from section A.

### Scan (section A)

- Keep `scan_order_rotates_and_wraps_for_resumable_scans` verbatim — it passes
  and it covers the `usize::MAX` wrap.
- **Truncated scans cover the whole fleet.** `n = 10`, 3 scanned per tick,
  `cursor = cursor.wrapping_add(scanned)` between ticks → after 4 ticks every
  index in `0..10` has been visited at least once. Locks the fairness claim.
- **A deferred tab survives the retain.** Build `seen_ids` from all 10 Claude
  tabs while only 3 were scanned; `watches.retain(…)` keeps the 7 unscanned
  watches. Cheap guard on the invariant that makes truncation safe.

### Unchanged

Keep every existing test as-is: the `scan_*` pattern set, `backoff_*`,
`should_nudge_*`, `round_robin_*`, `hash_output_*`, `connectivity_probe_*`,
`patterns_have_non_empty_labels_and_actions`,
`eligible_label_distinguishes_pattern_from_agent_error`.

Delete `two_level_breaker_full_freezes_on_api_storm_but_heartbeats_when_mild`
outright — it asserts the deadlocking behaviour is correct.

## Attribution

Force-pushing a cross-repo PR branch needs "allow edits by maintainers" on the
fork; otherwise open ours and close #40 with a pointer to this file.

Keep the original author credited as co-author on our commits:

```
Co-Authored-By: Amaury <amaury@terre-alternative.fr>
```

Do not re-litigate the review in the commit message — describe what the code
does, per normal repo convention.
