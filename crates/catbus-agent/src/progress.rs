// SPDX-License-Identifier: MPL-2.0

//! Deciding when a tool loop has stopped learning anything.
//!
//! On 2026-09-25 a `catbus-agent` tab burned 200 rounds and 2.55M input tokens
//! re-reading the same files. The relay had replaced old `tool_result` bodies
//! with `[elided:N B]` stubs to fit the request into the model's context window,
//! and a stub is indistinguishable from a real result the model has never read.
//! Its own words, from the transcript:
//!
//! > *I kept re-reading the same architectural files and getting `[elided: …]`
//! > back. Instead of narrowing, I issued more broad reads.*
//!
//! The relay side is fixed where it belongs: it no longer elides a body it has no
//! contextual reason to elide, and its stub now names the call it replaced. This
//! module is the client's own guard, for the runs where a model loops for any
//! other reason — a stale assumption, a file it cannot find, a tool that keeps
//! failing the same way.
//!
//! **What this module can and cannot see.** The client owns the calls it made and
//! the results its own tools returned. It does *not* own what the relay did to
//! those results on the wire, so it cannot count stubs — an earlier draft of this
//! file did exactly that, and it could never have fired. What it can see is the
//! signal underneath, which needs no knowledge of the relay at all: whether a
//! round changed the conversation, which is two questions asked together.
//!
//! # The two shapes of a stall
//!
//! * **The same call.** Identical tool, identical arguments, with a completed
//!   round in between. Left at that it would be wrong, because *the same call
//!   that keeps returning new output is a poll, and a poll is progress* — `make`
//!   run every thirty seconds until it goes green sends one identical `Bash` call
//!   every round and is exactly what an agent should do. A call is a loop only
//!   once its output has stopped changing too, so [`Progress::before`] requires
//!   the previous round's results to have repeated as well
//!   (see [`Progress::before`] for why that needs two result rounds, not one).
//!
//! * **The same result.** This is the shape the incident took, and it is why the
//!   module exists. The model did not repeat itself; it *widened*: a page, a
//!   wider page, then the whole file. Every call was different, so call identity
//!   says nothing. Nothing changed either — all three reads were the same file
//!   returning the same content — and asking for it a fourth way cannot return
//!   anything the third way did not. [`Progress::after`] compares results, which
//!   needs no knowledge of the relay and is the honest question: *did this round
//!   tell the model anything it did not already have?*
//!
//! # Cost
//!
//! The call check runs *before* dispatch, so a repeated call is refused rather
//! than run: no time spent, no side effect, no tokens. The result check can only
//! run after the tools have returned, so it stops the *next* round rather than the
//! round it judged — the results in hand are real and are recorded before the
//! loop ends, which is what keeps the turn valid for the next request.
//!
//! No I/O and no clock, so the whole policy is unit-testable.

use serde_json::Value;

/// How many consecutive rounds of no new information count as a loop.
///
/// Three, not two. Two rounds that look alike is ordinary work — edit then
/// re-read, test then re-test, ask again after fixing a typo, all produce exactly
/// that, and each changes what comes back. Three, with a whole completed round in
/// between each and the same bytes at the end of it, is a loop: nothing in the
/// conversation moved between the second and the third, so the third had nothing
/// left to show.
pub const DEFAULT_LIMIT: u32 = 3;

/// The keys a tool call names its target with, most specific first. Mirrors the
/// relay's list, so the two halves describe a call the same way.
const TARGET_KEYS: [&str; 6] = ["file_path", "path", "pattern", "command", "query", "name"];

/// How many characters of a call's target survive into a notice.
const TARGET_KEEP: usize = 48;

/// Why a round was stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The same calls, whole-round, [`DEFAULT_LIMIT`] times running, and their
    /// results have stopped changing.
    ///
    /// Caught before dispatch, so the calls were never made.
    Repeat { call: String, rounds: u32 },
    /// The same results, whole-round, [`DEFAULT_LIMIT`] times running.
    ///
    /// Caught after dispatch, so the results are real — they are simply identical
    /// to the ones the model already had.
    NoProgress { call: String, rounds: u32 },
}

impl Stop {
    /// The sentence shown to the user.
    ///
    /// Written for the person reading the tab: what happened, why running it
    /// again would not help, and what to do instead.
    #[must_use]
    pub fn notice(&self) -> String {
        match self {
            Self::Repeat { call, rounds } => format!(
                "\n\n[loop stopped after {rounds} identical rounds: `{call}` was about to run \
                 again, and the rounds before it had already stopped returning anything new. \
                 The call was not made. Ask for something specific to continue, or raise \
                 CATBUS_REPEAT_ROUNDS if the task is meant to be repetitive.]"
            ),
            Self::NoProgress { call, rounds } => format!(
                "\n\n[loop stopped after {rounds} rounds that changed nothing: the tools ran, \
                 and `{call}` was the last, but the results came back byte-identical each time. \
                 Running them again cannot produce anything new. Ask for something specific, or \
                 check whether the results are reaching the model at all — and raise \
                 CATBUS_REPEAT_ROUNDS if this was expected to be repetitive.]"
            ),
        }
    }

    /// The result handed back to the model in place of a call that was refused.
    ///
    /// A refused call still needs a `tool_result`: the Messages API requires one
    /// for every `tool_use` in the turn, and a bare `tool_use` makes the *next*
    /// request a 400. So the loop ends with a valid turn rather than an abandoned
    /// one.
    #[must_use]
    pub fn refusal(&self) -> String {
        match self {
            Self::Repeat { call, rounds } => format!(
                "Error: not run — `{call}` has already been dispatched {rounds} rounds running, \
                 and those rounds returned the same thing, so this one would return a result you \
                 have already been given. Try a different path, a narrower range, or ask the user."
            ),
            Self::NoProgress { call, rounds } => format!(
                "Error: the loop was stopped after {rounds} rounds in which every result came \
                 back byte-identical to the round before — `{call}` was the last call. Nothing \
                 changed, so repeating this cannot tell you anything new. Read something you \
                 have not read, or ask the user."
            ),
        }
    }
}

/// Tracks whether the tool loop is still making progress.
#[derive(Debug, Clone)]
pub struct Progress {
    limit: u32,
    /// The previous round's calls, canonically serialised, in order. `None` until
    /// the first round is observed.
    last_calls: Option<Vec<String>>,
    /// The previous round's results, in order.
    last_results: Option<Vec<String>>,
    /// The current round's calls as labels, for a notice that names the call.
    last_labels: Vec<String>,
    /// Consecutive rounds whose calls matched the round before them.
    call_streak: u32,
    /// Consecutive rounds whose results matched the round before them.
    result_streak: u32,
}

impl Progress {
    /// A tracker with an explicit threshold.
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self {
            limit,
            last_calls: None,
            last_results: None,
            last_labels: Vec::new(),
            call_streak: 0,
            result_streak: 0,
        }
    }

    /// A tracker whose threshold is `CATBUS_REPEAT_ROUNDS`, or [`DEFAULT_LIMIT`].
    ///
    /// The env-var escape hatch mirrors `CATBUS_MAX_ROUNDS`: a genuinely
    /// repetitive task is legitimate, and its owner should not have to patch the
    /// binary to run one. `0` disables the guard for that run. Most polling needs
    /// no escape at all — it passes on its own, because the thing being watched
    /// changes and so the results do; see the module docs.
    #[must_use]
    pub fn from_env() -> Self {
        let limit = std::env::var("CATBUS_REPEAT_ROUNDS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(DEFAULT_LIMIT);
        Self::new(limit)
    }

    /// Observe the calls a round is about to make, before dispatching them.
    ///
    /// Returns [`Stop::Repeat`] when this round repeats the previous one and the
    /// output has stopped changing. The caller must then refuse the calls rather
    /// than run them.
    ///
    /// Both halves of that condition are load-bearing. The call streak alone would
    /// stop a poll loop, whose calls are identical by nature and whose results are
    /// not — so it is gated on `result_streak >= 2`: the round just finished
    /// returned what the round before it did. Two result rounds are needed, not
    /// one, because a single round has nothing to compare against; it takes two to
    /// know the output has stalled. The consequence is that a repeat is only ever
    /// refused from the third round on, whatever the limit — you cannot know a
    /// repeat is fruitless before you have seen the same answer twice.
    pub fn before<'a>(&mut self, calls: impl Iterator<Item = (&'a str, &'a Value)>) -> Option<Stop> {
        if self.limit == 0 {
            return None;
        }
        let mut signature = Vec::new();
        let mut labels = Vec::new();
        for (name, input) in calls {
            labels.push(call_label(name, input));
            signature.push(format!("{name}:{}", canonical(input)));
        }
        // An empty round is the model ending its turn, not a loop: the caller
        // returns on it.
        if signature.is_empty() {
            return None;
        }
        self.call_streak = if self.last_calls.as_deref() == Some(signature.as_slice()) {
            self.call_streak + 1
        } else {
            1
        };
        self.last_calls = Some(signature);
        self.last_labels = labels;
        if self.call_streak >= self.limit && self.result_streak >= 2 {
            return Some(Stop::Repeat {
                call: first_label(&self.last_labels),
                rounds: self.call_streak,
            });
        }
        None
    }

    /// Observe a dispatched round's results, after they came back.
    ///
    /// Returns [`Stop::NoProgress`] when this round's results are byte-identical
    /// to the previous round's for the [`Self::limit`]-th time running. The caller
    /// should give the model no further round, but must still record the results
    /// it already has, so the turn stays valid.
    pub fn after<'a>(&mut self, results: impl Iterator<Item = &'a str>) -> Option<Stop> {
        let signature: Vec<String> = results.map(str::to_owned).collect();
        if self.limit == 0 || signature.is_empty() {
            return None;
        }
        self.result_streak = if self.last_results.as_deref() == Some(signature.as_slice()) {
            self.result_streak + 1
        } else {
            1
        };
        self.last_results = Some(signature);
        if self.result_streak >= self.limit {
            return Some(Stop::NoProgress {
                call: first_label(&self.last_labels),
                rounds: self.result_streak,
            });
        }
        None
    }
}

/// The label of a round's first call, or a placeholder for an empty round.
fn first_label(labels: &[String]) -> String {
    labels.first().cloned().unwrap_or_else(|| "the last call".to_owned())
}

/// "`Read /src/lib.rs`", or just "`Read`" when the call names no target.
///
/// The mirror of the relay's `describe_call`, and deliberately so: the notice the
/// user reads here and the stub the model reads there name a call the same way.
fn call_label(name: &str, input: &Value) -> String {
    let target = TARGET_KEYS
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str));
    match target.map(first_line_clipped) {
        Some(target) if !target.is_empty() => format!("{name} {target}"),
        _ => name.to_owned(),
    }
}

/// The first line of a target, clipped so a notice cannot inherit a whole prompt.
fn first_line_clipped(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= TARGET_KEEP {
        return line.to_owned();
    }
    let clipped: String = line.chars().take(TARGET_KEEP).collect();
    format!("{clipped}…")
}

/// A tool call's arguments, serialised with object keys in a fixed order.
///
/// Key order is the one thing that makes two identical calls look different: the
/// model re-emits `{"path": …, "offset": …}` and `{"offset": …, "path": …}` for
/// the same read, and a comparison that saw those as different would miss the
/// loop it exists to catch. `serde_json`'s `Value::Object` is a `BTreeMap` today,
/// so `to_string` would usually agree — "usually" is not a property a loop
/// detector should rest on, and this costs one pass over a small tree.
fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn calls<'a>(pairs: &'a [(&'a str, Value)]) -> impl Iterator<Item = (&'a str, &'a Value)> {
        pairs.iter().map(|(name, input)| (*name, input))
    }

    fn read(path: &str) -> Value {
        json!({"file_path": path})
    }

    /// Drive one whole round: the calls, then the results they returned.
    fn round(p: &mut Progress, call: (&str, Value), result: &str) -> Option<Stop> {
        let pair = [call];
        p.before(calls(&pair)).or_else(|| p.after(std::iter::once(result)))
    }

    /// `limit - 1` identical rounds followed by any change is not a loop. The
    /// guard must not be one round early: two rounds is the shape of real work.
    #[test]
    fn two_identical_rounds_do_not_trip() {
        let mut p = Progress::new(3);
        assert_eq!(round(&mut p, ("Read", read("/src/lib.rs")), "contents"), None);
        assert_eq!(
            round(&mut p, ("Read", read("/src/lib.rs")), "contents"),
            None,
            "two rounds is edit-then-re-test, not a loop"
        );
    }

    /// The stuck case: the same call, the same result. Refused *before* the third
    /// call runs — the point of checking on the way in.
    #[test]
    fn a_repeat_whose_output_stalled_is_refused_before_it_runs() {
        let mut p = Progress::new(3);
        assert_eq!(round(&mut p, ("Read", read("/src/lib.rs")), "contents"), None);
        assert_eq!(round(&mut p, ("Read", read("/src/lib.rs")), "contents"), None);
        let pair = [("Read", read("/src/lib.rs"))];
        let stop = p.before(calls(&pair)).expect("the third call is refused");
        match stop {
            Stop::Repeat { call, rounds } => {
                assert_eq!(call, "Read /src/lib.rs");
                assert_eq!(rounds, 3);
            }
            Stop::NoProgress { call, rounds } => {
                panic!("expected a repeat, got a no-progress stop on `{call}` after {rounds}")
            }
        }
    }

    /// The reason the call check is gated on the output stalling.
    ///
    /// A poll loop sends one identical call every round and is exactly what an
    /// agent should do — `make` until it goes green. Only the output tells the two
    /// apart, so a guard that trips on call identity alone would break the
    /// legitimate case while trying to fix the broken one. This test failed when
    /// it did.
    #[test]
    fn a_poll_whose_output_changes_is_never_a_loop() {
        let mut p = Progress::new(3);
        for i in 0..50 {
            let out = format!("build still running: {i} jobs");
            assert_eq!(
                round(&mut p, ("Bash", json!({"command": "make"})), &out),
                None,
                "round {i}: the call repeats, the output does not"
            );
        }
    }

    /// …and a poll whose output stops changing *is* a stall: nothing is moving.
    #[test]
    fn a_poll_whose_output_stops_changing_trips() {
        let mut p = Progress::new(3);
        assert_eq!(round(&mut p, ("Bash", json!({"command": "make"})), "up to date"), None);
        assert_eq!(round(&mut p, ("Bash", json!({"command": "make"})), "up to date"), None);
        let pair = [("Bash", json!({"command": "make"}))];
        assert!(
            p.before(calls(&pair)).is_some(),
            "three rounds of the same answer is a stall, however it was reached"
        );
    }

    /// The shape the guard is really for: the calls *change*, the content does
    /// not. This is what the 2026-09-25 tab did — "instead of narrowing, I issued
    /// more broad reads" — and a call-identity check alone cannot see it.
    #[test]
    fn varied_calls_returning_the_same_content_trip() {
        let mut p = Progress::new(3);
        // Three different reads of one file: a page, a wider page, the lot.
        let shapes = [
            json!({"file_path": "/src/lib.rs", "limit": 200}),
            json!({"file_path": "/src/lib.rs", "limit": 400}),
            json!({"file_path": "/src/lib.rs"}),
        ];
        let same = "the whole file, regardless of how it was asked for";
        for (index, shape) in shapes.iter().enumerate() {
            let stop = round(&mut p, ("Read", shape.clone()), same);
            if index < 2 {
                assert_eq!(stop, None, "round {index} is not yet a run of three");
            } else {
                match stop {
                    Some(Stop::NoProgress { call, rounds }) => {
                        assert_eq!(call, "Read /src/lib.rs", "named by the last call");
                        assert_eq!(rounds, 3);
                    }
                    Some(Stop::Repeat { call, rounds }) => {
                        panic!("expected a no-progress stop, got a repeat on `{call}` after {rounds}")
                    }
                    None => panic!("round {index} should have been the stop"),
                }
            }
        }
    }

    /// A different call resets the call streak — and what happens next depends
    /// entirely on the results, which is the whole reason the two streaks are
    /// tracked separately.
    ///
    /// Same call, new content: a search, and it runs as long as it takes. Same
    /// call, same content: a stall. The two cases are indistinguishable from the
    /// calls alone, which is why a guard built on call identity would either break
    /// the first or miss the second.
    #[test]
    fn a_new_call_with_new_content_is_a_search_but_the_same_content_is_a_stall() {
        // A search: every round reads somewhere else and learns something.
        let mut p = Progress::new(3);
        for (index, path) in ["/src/a.rs", "/src/b.rs", "/src/c.rs", "/src/d.rs", "/src/e.rs"]
            .iter()
            .enumerate()
        {
            let out = format!("contents of {path}");
            assert_eq!(
                round(&mut p, ("Read", read(path)), &out),
                None,
                "round {index} of a search"
            );
        }

        // A stall behind varying calls: same file, asked for three ways.
        let mut p = Progress::new(3);
        let same = "contents";
        assert_eq!(round(&mut p, ("Read", read("/src/a.rs")), same), None);
        assert_eq!(round(&mut p, ("Read", read("/src/a.rs")), same), None);
        // b is a new call, so the *call* streak restarts at 1 — but the result
        // streak is what trips here, because b handed back the same content.
        assert_eq!(
            round(&mut p, ("Read", read("/src/b.rs")), same),
            Some(Stop::NoProgress {
                call: "Read /src/b.rs".into(),
                rounds: 3,
            })
        );
    }

    /// A round that mixes a new call into a repeat is doing something new. The
    /// guard compares whole rounds, or a model that re-reads one file while
    /// opening another would look like a loop.
    ///
    /// The counterfactual is the point: from this state the *same* round would be
    /// refused, so the guard is armed and still declines to fire on the added
    /// call. Without that half, the test would pass on a guard that had simply
    /// stopped working.
    #[test]
    fn a_round_that_adds_a_call_is_not_a_repeat() {
        let mut p = Progress::new(3);
        let one = [("Read", read("/src/a.rs"))];
        let both = [("Read", read("/src/a.rs")), ("Read", read("/src/b.rs"))];
        assert_eq!(p.before(calls(&one)), None);
        assert_eq!(p.after(std::iter::once("same")), None);
        // Two rounds in, call streak 2 and result streak 2 — one short of the
        // limit, so nothing has fired yet and the state below is reachable.
        assert_eq!(p.before(calls(&one)), None);
        assert_eq!(p.after(std::iter::once("same")), None);

        let mut same = p.clone();
        assert!(
            same.before(calls(&one)).is_some(),
            "a third identical round, on stalling output, is refused"
        );
        // The same state, one added call: different work, not a loop.
        assert_eq!(p.before(calls(&both)), None, "the round is not the same round");
    }

    /// Key order is not a difference between two calls.
    #[test]
    fn key_order_does_not_hide_a_repeat() {
        let mut p = Progress::new(3);
        let first = [("Read", json!({"file_path": "/src/lib.rs", "offset": 10}))];
        let reordered = [("Read", json!({"offset": 10, "file_path": "/src/lib.rs"}))];
        assert_eq!(p.before(calls(&first)), None);
        assert_eq!(p.after(std::iter::once("same")), None);
        assert_eq!(p.before(calls(&reordered)), None, "streak two");
        assert_eq!(p.after(std::iter::once("same")), None);
        assert!(
            p.before(calls(&reordered)).is_some(),
            "the same read with its keys reordered is still the same read"
        );
    }

    /// One result that changed is progress. A model that is half-blind is still
    /// learning, and cutting it would be the guard eating good work.
    #[test]
    fn one_changed_result_resets_the_streak() {
        let mut p = Progress::new(2);
        assert_eq!(p.after(["a", "b"].into_iter()), None);
        assert_eq!(p.after(["a", "c"].into_iter()), None, "c is new information");
        assert_eq!(p.after(["a", "c"].into_iter()).map(|_| ()), Some(()));
    }

    /// An empty round is the model ending its turn, not a loop.
    #[test]
    fn an_empty_round_is_never_a_stop() {
        let mut p = Progress::new(1);
        let none: [(&str, Value); 0] = [];
        assert_eq!(p.before(calls(&none)), None);
        assert_eq!(p.after(std::iter::empty()), None);
    }

    /// The escape hatch, for a task that is legitimately repetitive.
    #[test]
    fn a_zero_limit_disables_the_guard() {
        let mut p = Progress::new(0);
        let a = [("Bash", json!({"command": "cargo test"}))];
        for _ in 0..10 {
            assert_eq!(p.before(calls(&a)), None);
            assert_eq!(p.after(std::iter::once("same")), None);
        }
    }

    /// A call with no target key is still named, by its tool.
    #[test]
    fn a_targetless_call_is_labelled_by_its_tool() {
        assert_eq!(call_label("TodoWrite", &json!({"todos": []})), "TodoWrite");
        assert_eq!(
            call_label("Bash", &json!({"command": "cargo test\necho hi"})),
            "Bash cargo test",
            "a multi-line command is labelled by its first line"
        );
    }

    /// Both notices tell the reader what happened; both refusals tell the model
    /// what to do instead. A truncated or empty refusal would leave the model with
    /// a `tool_result` it cannot act on.
    #[test]
    fn the_two_stops_explain_themselves() {
        let repeat = Stop::Repeat {
            call: "Read /a.rs".into(),
            rounds: 3,
        };
        assert!(repeat.notice().contains("3 identical rounds"), "{}", repeat.notice());
        assert!(
            repeat.notice().contains("CATBUS_REPEAT_ROUNDS"),
            "the notice must name the knob: {}",
            repeat.notice()
        );
        assert!(repeat.refusal().starts_with("Error: not run"), "{}", repeat.refusal());

        let stuck = Stop::NoProgress {
            call: "Read /b.rs".into(),
            rounds: 3,
        };
        assert!(stuck.notice().contains("changed nothing"), "{}", stuck.notice());
        assert!(
            stuck.notice().contains("byte-identical"),
            "the notice must say what was seen, not claim to know why: {}",
            stuck.notice()
        );
        assert!(
            stuck.notice().contains("CATBUS_REPEAT_ROUNDS"),
            "the notice must name the knob: {}",
            stuck.notice()
        );
        assert!(stuck.refusal().contains("Read /b.rs"), "{}", stuck.refusal());
    }
}
