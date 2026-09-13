// SPDX-License-Identifier: MPL-2.0

//! Interactive tool policy: what `tools[]` this client actually gets.
//!
//! The design and its reasoning live in `docs/proxy-tools.md`; this is the
//! implementation of it. Three verbs, applied to every request from an
//! account:
//!
//! * **disable** — remove tools the client sent.
//! * **whitelist** — send only these (mode `allow`).
//! * **rewrite** — add tools the client did not send (mode `add`).
//!
//! The rule that makes it safe is *referenced ∪ pins*: a tool name that
//! already appears in `messages[]` must keep its definition, whichever mode
//! is in force. Anthropic validates that pairing, and a definition the client
//! has already called cannot simply vanish mid-conversation.
//!
//! The default — mode `all`, every list empty — is a faithful no-op. That is
//! load-bearing, not a convenience: an unconfigured account must produce a
//! byte-identical body.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How far back to look for referenced names.
///
/// Matches compaction's window rather than being its own number, because the
/// two answer the same question — which of these are still live — and
/// disagreeing windows would mean a tool kept for one reason and dropped for
/// another in the same pass.
pub const PINS_WINDOW: usize = 24;

/// Always kept, in every mode, whatever it is referenced by.
///
/// Not a policy choice: tool search is how the client reaches every other
/// tool, so removing it does not hide one tool, it hides the mechanism.
pub const ALWAYS_KEPT: [&str; 1] = ["ToolSearch"];

/// How many tools the client is offered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything the client sent, minus `disable`. The default.
    #[default]
    All,
    /// Everything on the first request, then only names the conversation
    /// has actually called.
    Referenced,
    /// Only `allow`, in `allow` order.
    Allow,
    /// Nothing, except names the conversation has called.
    None,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Referenced => "referenced",
            Self::Allow => "allow",
            Self::None => "none",
        }
    }

    /// Whether this mode can remove anything at all. Used to skip the whole
    /// pass — and the re-encode it would force — for the default case.
    #[must_use]
    const fn prunes(self) -> bool {
        !matches!(self, Self::All)
    }
}

/// One account's tool policy.
///
/// Every field defaults, so an account file written before this existed
/// loads as the no-op, and `deny_unknown_fields` is deliberately *not* set:
/// an unrecognized key is a typo, but failing the whole load over one would
/// take the proxy down rather than the tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub mode: Mode,
    /// Names to remove, in every mode.
    ///
    /// A removed tool still has its `tool_use` blocks in `messages[]`, and
    /// Anthropic rejects a `tool_use` whose tool is not defined. So this is
    /// about the *initial* surface only: it cannot erase history, and a name
    /// the conversation has already called is pinned regardless.
    pub disable: Vec<String>,
    /// The complete set, for `mode: allow`.
    pub allow: Vec<String>,
    /// Definitions to add. Appended after the client's, and never allowed to
    /// shadow one — see [`Refusal::Shadow`].
    pub add: Vec<Value>,
    /// Description edits, keyed by tool name.
    ///
    /// A tool the client sent keeps its name, its schema and its position, so
    /// this moves nothing a cache mark points at and cannot orphan a
    /// `tool_use`. That is the whole reason it exists: it takes volatility out
    /// of `tools[]` without paying the re-warm that `disable` costs.
    ///
    /// Only the description is editable. A name is what `messages[]` refers
    /// to, and the schema is what the model calls the tool with; either one
    /// rewritten on the way through is the proxy inventing a contract.
    pub rewrite: BTreeMap<String, Vec<Normalise>>,
}

/// Why a requested change did not happen. Surfaced rather than swallowed:
/// a policy that silently does nothing is indistinguishable from a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An `add` entry names a tool the client already sent. Refused because
    /// an added definition can describe a *new* capability, but one that
    /// shadows a native tool would be the proxy silently redefining what
    /// `Read` or `Bash` means.
    Shadow(String),
    /// An `add` entry has no `name`, so it can never be called and would
    /// only be rejected upstream.
    Nameless,
}

impl Refusal {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Shadow(name) => format!("`{name}` already exists; add cannot redefine it"),
            Self::Nameless => "an added tool has no `name`".to_owned(),
        }
    }
}

/// A named edit to a tool description.
///
/// Named rather than an operator-supplied expression on purpose. This runs on
/// every request and its output is a prefix the provider caches against, so a
/// rule that is subtly wrong is both a correctness bug and a silent cache
/// invalidator. A closed set can be tested against the text it will actually
/// meet; a regex cannot, and the crate carries none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Normalise {
    /// Drop sentences asserting the current date.
    ///
    /// A description that announces "the current month" is dated the day it
    /// ships and hostile to caching thereafter: the same tool arrives with a
    /// different prefix each month, and the first request after the boundary
    /// pays a full re-warm. The instruction it decorates is the point; the
    /// date is not.
    Dates,
    /// Drop sentences asserting the provider's locality.
    ///
    /// "US-only" describes Anthropic's search backend. It travels unchanged to
    /// a hop that is not Anthropic, where it is false — the proxy putting one
    /// provider's fact into a request for another. See the worked case in
    /// `docs/proxy-tools.md`.
    Provider,
    /// Swap one fixed string for another, everywhere it appears.
    ///
    /// The bluntest rule in the set and the only one carrying operator input,
    /// which is exactly why it is a pair of literals in this closed enum and
    /// not an expression: a phrase goes in, a phrase comes out, nothing else.
    /// An empty `replace` deletes the phrase.
    Replaced { find: String, replace: String },
}

impl Normalise {
    /// Apply to a description, or `None` if it already reads that way.
    ///
    /// Returning `None` rather than a copy keeps the caller honest: an edit
    /// that changes nothing must not be counted, or a description would be
    /// rewritten to itself on every request and reported as churn.
    #[must_use]
    pub fn run(&self, description: &str) -> Option<String> {
        let pruned = match self {
            Self::Dates => prune_sentences(description, claims_date),
            Self::Provider => prune_sentences(description, claims_provider),
            Self::Replaced { find, replace } => {
                if find.is_empty() || !description.contains(find.as_str()) {
                    return None;
                }
                // A rule that replaces a string with itself is a no-op, and
                // counting it would report churn that did not happen.
                let next = description.replace(find.as_str(), replace);
                return if next == description { None } else { Some(next) };
            }
        };
        if pruned == description { None } else { Some(pruned) }
    }
}

/// Drop every sentence `claimed`, then tidy the text the hole left behind.
///
/// Sentence-level rather than line-level: the volatile text sits at the end of
/// a prose paragraph that also carries the instruction, so dropping the line
/// would drop the instruction with it.
fn prune_sentences(text: &str, claimed: fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut dropped = false;
    for chunk in text.split_inclusive('\n') {
        let (line, newline) = chunk.strip_suffix('\n').map_or((chunk, ""), |line| (line, "\n"));
        for sentence in split_sentences(line) {
            if claimed(sentence) {
                dropped = true;
            } else {
                out.push_str(sentence);
            }
        }
        out.push_str(newline);
    }
    if !dropped {
        return text.to_owned();
    }
    // A dropped sentence can leave a line empty (the paragraph was only that
    // sentence) or trailing whitespace where the sentence's own gap went.
    collapse(&out)
}

/// Split a line at sentence ends, keeping each terminator and the gap after
/// it, so concatenating the pieces reproduces the line byte for byte.
///
/// A period inside an abbreviation therefore reads as an end too. That is
/// acceptable here because it can only cost precision — a fragment left
/// behind still reads as English, and the alternative is an abbreviation list
/// this crate has no business carrying.
fn split_sentences(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let mut end = i + 1;
            while end < bytes.len() && bytes[end] == b' ' {
                end += 1;
            }
            parts.push(&line[start..end]);
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    if start < line.len() {
        parts.push(&line[start..]);
    }
    parts
}

/// Trim each line's tail and squeeze runs of blank lines to one.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for chunk in text.split_inclusive('\n') {
        let (line, newline) = chunk.strip_suffix('\n').map_or((chunk, ""), |line| (line, "\n"));
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push_str(newline);
    }
    out
}

/// Whether a sentence asserts which month or year it is now.
fn claims_date(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    if DATE_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }
    // A month name alone is not a claim — "you may share this" is not about
    // May — so the year has to be there too. The reverse is not enough
    // either: "HTTP 200" and a copyright year carry no dates a model reads.
    names_month(sentence) && has_year(sentence)
}

/// Whether a sentence asserts the provider's locality.
///
/// Matched without regard to case, and both spellings of each: the text these
/// come from is machine-written prose, and a rule that misses "US-based"
/// because it was written "US based" would fail exactly where it is needed.
fn claims_provider(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    PROVIDER_CLAIMS.iter().any(|claim| lower.contains(claim))
}

const DATE_PHRASES: [&str; 4] = ["current month", "current date", "today's date", "todays date"];

const PROVIDER_CLAIMS: [&str; 4] = ["us-only", "us only", "us-based", "us based"];

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Whether the text names a month as a word, so that "May" counts and
/// "maybe" does not.
fn names_month(text: &str) -> bool {
    MONTHS.iter().any(|month| contains_word(text, month))
}

/// Whether the text carries a four-digit year.
///
/// A run of exactly four digits, so a fragment of a longer number or a
/// decimal does not read as one, and bounded to the range a date could
/// plausibly use so a 4-digit id does not either.
fn has_year(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i - start == 4
            && text[start..i]
                .parse::<u32>()
                .is_ok_and(|year| (1900..=2999).contains(&year))
        {
            return true;
        }
    }
    false
}

/// Case-insensitive `contains`, with word boundaries on both sides.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let len = needle.len();
    if len == 0 || len > hay.len() {
        return false;
    }
    for i in 0..=hay.len() - len {
        if !hay[i..i + len].eq_ignore_ascii_case(needle.as_bytes()) {
            continue;
        }
        let before = i == 0 || !hay[i - 1].is_ascii_alphanumeric();
        let after = i + len == hay.len() || !hay[i + len].is_ascii_alphanumeric();
        if before && after {
            return true;
        }
    }
    false
}

/// What a pass did, for the log line and the panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub mode: Mode,
    /// How many definitions the client sent.
    pub offered: usize,
    /// How many were sent on.
    pub sent: usize,
    /// Names kept against the mode or against `disable`.
    pub pinned: Vec<String>,
    /// Names dropped.
    pub removed: Vec<String>,
    pub added: usize,
    pub refused: Vec<Refusal>,
    /// Names whose description a rewrite edited.
    pub rewritten: Vec<String>,
    /// Names the proxy resolves itself, so they are neither forwarded as
    /// declarations nor counted as added.
    pub local: Vec<String>,
    /// `cache_control` marks dropped because the provider cannot take them.
    pub cache_stripped: bool,
}

impl Report {
    #[must_use]
    pub const fn changed(&self) -> bool {
        !self.removed.is_empty()
            || self.added > 0
            || !self.refused.is_empty()
            || !self.rewritten.is_empty()
            || !self.local.is_empty()
            || self.cache_stripped
    }
}

/// Whether this policy could change any request. The default cannot, and
/// `shape_body` uses that to stay out of the way entirely.
///
/// No `allow` term, deliberately. `allow` is read only in [`Mode::Allow`],
/// which already prunes — so `!prunes()` has answered "no" before the list
/// could be consulted, and the term would be dead code that reads as if it
/// were doing work. The tempting misreading it would invite is the opposite of
/// the truth: a populated `allow` under [`Mode::All`] is still a no-op, because
/// `All` ignores the list. Such a policy is stored and means something the
/// moment the mode changes; it just does not act now.
#[must_use]
pub fn is_noop(policy: &Policy) -> bool {
    !policy.mode.prunes() && policy.disable.is_empty() && policy.add.is_empty() && policy.rewrite.is_empty()
}

/// Reject a policy that could never do what it says, before it is stored.
///
/// Only the mistakes that are certain. `add` shadowing a native tool depends on
/// what the client sends, so it stays a per-request [`Refusal`]; a policy that
/// merely surprises its author does not get vetoed here. What is caught is the
/// empty name — it matches no tool and can never become non-empty, so a policy
/// holding one is a typo that would otherwise be silently inert forever, which
/// is exactly the failure [`Refusal`] exists to prevent.
///
/// # Errors
/// The name of the offending entry.
pub fn validate(policy: &Policy) -> Result<(), String> {
    let empty = |what: &str, name: &str| format!("{what} entry {name:?} has no name");
    for name in policy.disable.iter().chain(&policy.allow) {
        if name.trim().is_empty() {
            return Err(empty("a disable/allow", name));
        }
    }
    for entry in &policy.add {
        if entry
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(|n| n.trim().is_empty())
        {
            return Err("an add entry needs a non-empty `name`".to_owned());
        }
    }
    for (tool, rules) in &policy.rewrite {
        if tool.trim().is_empty() {
            return Err("a rewrite entry has no tool name".to_owned());
        }
        // Two keys differing only by case would resolve to whichever the map
        // happens to iterate first, making the other one inert. `rules_for`
        // matches loosely, so this is the only place the collision can show.
        if policy
            .rewrite
            .keys()
            .filter(|other| other.eq_ignore_ascii_case(tool))
            .count()
            > 1
        {
            return Err(format!("two rewrite entries name {tool:?} in different cases"));
        }
        // A `Replaced` with nothing to find replaces nothing. It is inert, so
        // it would sit in the stored policy looking like it does something.
        if rules.iter().any(|rule| match rule {
            Normalise::Replaced { find, .. } => find.is_empty(),
            Normalise::Dates | Normalise::Provider => false,
        }) {
            return Err(format!("a rewrite of {tool:?} finds an empty string"));
        }
    }
    Ok(())
}

/// Apply `policy` to a request body in place.
///
/// `takes_cache` is whether the provider accepts `cache_control` on a tool
/// definition. Rebuilding `tools[]` moves the client's cache breakpoints, so
/// when the array is rewritten and the provider cannot hold them, the marks
/// are dropped — a refused request is worse than a cold cache. `messages[]`
/// is left alone: those marks are not ours to remove.
///
/// Does nothing to a body whose `tools` is absent or not an array.
pub fn apply(body: &mut Value, policy: &Policy, takes_cache: bool) -> Report {
    let mut report = Report {
        mode: policy.mode,
        ..Report::default()
    };
    if is_noop(policy) {
        return report;
    }
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        // No tools to govern. `add` still applies — injection does not need
        // an existing array — but nothing else does.
        return add_only(body, policy, takes_cache, report);
    };

    report.offered = tools.len();
    let first_turn = is_first_turn(body);
    let mut keep = referenced_names(body);
    // A forced tool is a pin the body states outright. Folded in here rather
    // than given an arm of its own so it inherits the same protection — and so
    // `disable` cannot win against it, which is the whole reason it is here.
    if let Some(forced) = forced_name(body) {
        keep.insert(forced.to_owned());
    }
    let disabled: std::collections::HashSet<&str> = policy.disable.iter().map(String::as_str).collect();

    let mut sent: Vec<Value> = Vec::with_capacity(tools.len());
    for tool in tools {
        let Some(name) = tool_name(tool) else {
            // A definition with no name is not ours and not actionable;
            // forwarding it is the only thing that cannot make it worse.
            sent.push(tool.clone());
            continue;
        };
        let protected = ALWAYS_KEPT.contains(&name) || keep.contains(name);
        let wanted = keep_by_mode(policy, name, first_turn) && !disabled.contains(name);
        if wanted || protected {
            if !wanted {
                // Kept against the mode, or against `disable`. This is the
                // case the whole referenced-union rule exists for: dropping
                // it would leave a `tool_use` in `messages[]` naming a tool
                // that is no longer defined, which upstream rejects outright.
                report.pinned.push(name.to_owned());
            }
            sent.push(tool.clone());
        } else {
            report.removed.push(name.to_owned());
        }
    }

    // `allow` is an ordering instruction as well as a filter.
    if policy.mode == Mode::Allow {
        sent.sort_by_key(|tool| {
            tool_name(tool)
                .and_then(|name| policy.allow.iter().position(|a| a == name))
                .unwrap_or(usize::MAX)
        });
    }

    apply_rewrites(&mut sent, policy, &mut report);

    report.sent = sent.len();
    // A cache mark on a definition this client sent, when the array has been
    // rebuilt around it, no longer points where the client put it.
    if !takes_cache && report.changed_before_cache() {
        strip_cache_control(&mut sent);
        report.cache_stripped = true;
    }

    let client_names: std::collections::HashSet<String> = sent
        .iter()
        .filter_map(|t| tool_name(t).map(ToOwned::to_owned))
        .collect();
    append_added(&mut sent, policy, &client_names, !takes_cache, &mut report);
    // Last, so `sent` is what actually goes on the wire — offerings plus
    // whatever `add` contributed, not just the client's survivors.
    report.sent = sent.len();

    body["tools"] = Value::Array(sent);
    report
}

/// `add` against a body that has no `tools[]` at all.
///
/// Injection does not need an existing array — that is the whole point of
/// it — so this shares the refusal rules with the main path rather than
/// skipping them.
fn add_only(body: &mut Value, policy: &Policy, takes_cache: bool, mut report: Report) -> Report {
    let mut sent: Vec<Value> = Vec::new();
    let client_names = std::collections::HashSet::new();
    reject_and_append(
        &mut sent,
        policy.add.iter().cloned(),
        &client_names,
        !takes_cache,
        &mut report,
    );
    report.cache_stripped = !takes_cache && report.added > 0;
    report.sent = sent.len();
    if !sent.is_empty() {
        body["tools"] = Value::Array(sent);
    }
    report
}

fn append_added(
    sent: &mut Vec<Value>,
    policy: &Policy,
    client_names: &std::collections::HashSet<String>,
    strip_marks: bool,
    report: &mut Report,
) {
    reject_and_append(sent, policy.add.iter().cloned(), client_names, strip_marks, report);
}

/// The rules for a tool, matched the way every other name in this engine is:
/// case-insensitively.
///
/// A `rewrite` keyed exactly would make `{"WebSearch": …}` a silent no-op
/// against a client's `websearch`, which is the one failure a policy must not
/// have — the same reason `disable` and `add` compare loosely.
fn rules_for<'a>(policy: &'a Policy, name: &str) -> Option<&'a [Normalise]> {
    policy
        .rewrite
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, rules)| rules.as_slice())
}

/// Runs each client-sent definition's name through its rules, editing the
/// description where it already sits.
///
/// A tool with no description is left alone. There is no prose to normalise,
/// and inventing one would be a bigger change than the policy asked for.
fn apply_rewrites(sent: &mut [Value], policy: &Policy, report: &mut Report) {
    for tool in sent {
        let Some(name) = tool_name(tool).map(ToOwned::to_owned) else {
            continue;
        };
        let Some(rules) = rules_for(policy, &name) else {
            continue;
        };
        let Some(description) = tool.get("description").and_then(Value::as_str) else {
            continue;
        };
        let mut text = description.to_owned();
        let mut edited = false;
        for rule in rules {
            if let Some(next) = rule.run(&text) {
                text = next;
                edited = true;
            }
        }
        if edited {
            tool["description"] = Value::String(text);
            report.rewritten.push(name);
        }
    }
}

/// The shadow rule and the nameless rule, in one place so `add_only` and the
/// main path cannot drift apart.
///
/// `strip_marks` is the provider's opinion, not the policy's: a definition we
/// are the ones adding may as well not carry a cache mark the upstream will
/// refuse.
fn reject_and_append(
    sent: &mut Vec<Value>,
    incoming: impl Iterator<Item = Value>,
    client_names: &std::collections::HashSet<String>,
    strip_marks: bool,
    report: &mut Report,
) {
    let mut seen: std::collections::HashSet<String> = sent
        .iter()
        .filter_map(|t| tool_name(t).map(ToOwned::to_owned))
        .collect();
    for tool in incoming {
        let Some(name) = tool_name(&tool).map(ToOwned::to_owned) else {
            report.refused.push(Refusal::Nameless);
            continue;
        };
        if crate::localtool::is_local(&name) {
            // The proxy answers this one itself, so the declaration must not be
            // forwarded: the far end has no such tool and refuses the whole body
            // over the unknown type. The name is enough to resolve it.
            // ponytail: the injected entry has no description the model could
            // read before the result arrives; if a local tool ever needs one,
            // give it a definition here rather than in the policy.
            report.local.push(name);
            continue;
        }
        if client_names.contains(&name) || seen.contains(&name) {
            report.refused.push(Refusal::Shadow(name));
            continue;
        }
        seen.insert(name);
        report.added += 1;
        let mut tool = tool;
        if strip_marks {
            strip_one(&mut tool);
        }
        sent.push(tool);
    }
}

impl Report {
    /// Whether anything was removed or refused before the cache decision.
    /// `added` is not counted: an addition does not move the client's marks.
    /// Whether a change moved a definition out from under a cache mark.
    ///
    /// A rewrite is deliberately not one. It edits a definition where it
    /// already sits, so the mark still points at the tool it was put on, and
    /// the whole reason to rewrite is to stop paying a re-warm — counting it
    /// here would strip the mark and buy back the churn the rule exists to
    /// remove.
    const fn changed_before_cache(&self) -> bool {
        !self.removed.is_empty()
    }
}

fn strip_cache_control(tools: &mut [Value]) {
    for tool in tools {
        strip_one(tool);
    }
}

fn strip_one(tool: &mut Value) -> bool {
    tool.as_object_mut()
        .is_some_and(|o| o.remove("cache_control").is_some())
}

/// Whether the mode alone would offer this tool, before pins and exemptions.
///
/// A separate function because the pin rule is not a mode: it is a floor
/// beneath every mode, so the modes must be answerable on their own terms.
fn keep_by_mode(policy: &Policy, name: &str, first_turn: bool) -> bool {
    match policy.mode {
        // Everything, always — not "everything to begin with". `all` and
        // `referenced` are easy to conflate here and the two arms look
        // mergeable; they are not, and doing so silently turns `all` into
        // `referenced` from the second turn onward.
        Mode::All => true,
        // Everything on the opening request, then only what has been
        // called. The first turn is the whole point: nothing has been
        // called yet, so pruning there would leave the model with no
        // tools at all.
        Mode::Referenced => first_turn,
        Mode::Allow => policy.allow.iter().any(|a| a == name),
        Mode::None => false,
    }
}

#[must_use]
pub fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

/// Whether this is the client's opening request.
///
/// The proxy cannot see "the first request of a session" directly, but it
/// does not need to: a conversation that has not answered anything yet has
/// no `assistant` message. Every later request in the same conversation
/// carries the transcript, so the first `assistant` turn is the moment the
/// window starts applying.
fn is_first_turn(body: &Value) -> bool {
    !messages(body).any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
}

/// Tool names the conversation has already called, over the tail window.
///
/// These are the definitions that must survive whatever the mode says: a
/// `tool_use` block naming a tool that is not in `tools[]` is rejected
/// upstream, so removing one would break the request rather than the tool.
///
/// The [`ALWAYS_KEPT`] exemptions are deliberately *not* folded in here.
/// They are a different reason to keep a tool, and merging them would make
/// `Report::pinned` unable to say which one applied.
fn referenced_names(body: &Value) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let all: Vec<&Value> = messages(body).collect();
    let start = all.len().saturating_sub(PINS_WINDOW);
    for message in &all[start..] {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let Some(name) = block.get("name").and_then(Value::as_str) {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

fn messages(body: &Value) -> impl Iterator<Item = &Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .map(std::vec::Vec::as_slice)
        .unwrap_or_default()
        .iter()
}

/// The tool `tool_choice` forces, if it names one.
///
/// `tool_choice: {"type":"tool","name":"X"}` is a demand, not a hint: upstream
/// rejects the request if `X` is absent from `tools[]`. It reads as the same
/// kind of thing as a pin — a name that must survive — but it arrives somewhere
/// neither of the other two guards looks, so a policy that filters perfectly
/// still 400s on the one request that used it.
///
/// Only the named form. `auto`, `any` and `none` name nothing, and `any`
/// merely requires *some* tool to remain, which is the one case a policy that
/// empties the array breaks regardless of what this returns.
fn forced_name(body: &Value) -> Option<&str> {
    let choice = body.get("tool_choice")?;
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    choice.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": format!("the {name} tool"),
               "input_schema": {"type": "object"}})
    }

    fn names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| tool_name(t).map(ToOwned::to_owned)).collect())
            .unwrap_or_default()
    }

    fn request(tools: &[&str]) -> Value {
        json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": tools.iter().map(|n| tool(n)).collect::<Vec<_>>(),
        })
    }

    /// The load-bearing case: an account that configured nothing must not
    /// have its request touched at all.
    #[test]
    fn the_default_policy_is_a_faithful_noop() {
        let policy = Policy::default();
        assert!(is_noop(&policy));
        let mut body = request(&["Read", "Bash"]);
        let before = body.clone();
        let report = apply(&mut body, &policy, true);
        assert_eq!(body, before, "body must be byte-identical");
        assert!(!report.changed());
    }

    #[test]
    fn a_noop_policy_is_also_a_noop_when_the_provider_cannot_cache() {
        let mut body = request(&["Read"]);
        let before = body.clone();
        apply(&mut body, &Policy::default(), false);
        assert_eq!(body, before, "an untouched array is not rebuilt, so no stripping");
    }

    /// `allow` belongs to one mode and is dead weight in the others. Locked
    /// down because the other reading is plausible enough to "fix" into
    /// existence: a populated `allow` under `all` looks like a policy that
    /// should run, and making it run would re-encode the body — reordering
    /// every key through `serde_json` — to accomplish nothing.
    #[test]
    fn an_allow_list_under_the_mode_that_ignores_it_is_still_a_noop() {
        let policy = Policy {
            mode: Mode::All,
            allow: vec!["Read".into(), "Bash".into()],
            ..Policy::default()
        };
        assert!(is_noop(&policy), "`all` never consults the list");
        let mut body = request(&["Read", "Bash", "WebFetch"]);
        let before = body.clone();
        let report = apply(&mut body, &policy, true);
        assert_eq!(body, before, "byte-identical, not merely equivalent");
        assert!(!report.changed());
    }

    /// …but the same list is not a no-op under the mode that reads it, which
    /// is the half that keeps the test above from proving too much.
    #[test]
    fn the_same_allow_list_acts_once_the_mode_reads_it() {
        let policy = Policy {
            mode: Mode::Allow,
            allow: vec!["Read".into(), "Bash".into()],
            ..Policy::default()
        };
        assert!(!is_noop(&policy));
        let mut body = request(&["Read", "Bash", "WebFetch"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(report.removed, vec!["WebFetch".to_owned()]);
        assert_eq!(report.sent, 2);
    }

    #[test]
    fn disable_removes_the_named_tools() {
        let policy = Policy {
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Edit"]);
        assert_eq!(report.removed, ["Bash"]);
        assert_eq!(report.offered, 3);
        assert_eq!(report.sent, 2);
    }

    /// `all` is not `referenced`. Mid-conversation it must still offer
    /// everything the client sent, not just what has been called — the two
    /// are easy to conflate, so this case gets its own test.
    #[test]
    fn mode_all_keeps_everything_even_mid_conversation() {
        let policy = Policy {
            disable: vec!["Grep".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Grep"]);
        body["messages"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            ]},
        ]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Bash"], "the uncalled one survives too");
        assert_eq!(report.removed, ["Grep"]);
        assert!(report.pinned.is_empty(), "nothing needed pinning");
    }

    /// A name the conversation has called must keep its definition even when
    /// `disable` names it — removing it would produce a `tool_use` with no
    /// matching tool, which is a rejection, not a hiding.
    #[test]
    fn a_disabled_tool_still_survives_if_the_conversation_called_it() {
        // The window is what makes this gentle, so the call has to be inside
        // it: history the extractor would have dropped is history we do not
        // have to protect.
        let policy = Policy {
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["messages"] = json!([
            {"role": "user", "content": "run it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]},
        ]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Bash"], "pinned, so not removed");
        assert!(report.removed.is_empty());
        assert_eq!(report.pinned, ["Bash"]);
    }

    #[test]
    fn a_forced_tool_choice_outranks_the_mode_and_disable() {
        // Upstream rejects a request whose `tool_choice` names a tool that is
        // not in `tools[]`. Whichever way the policy would have gone, this
        // name has to come out the other side.
        let policy = Policy {
            mode: Mode::None,
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit"]);
        body["tool_choice"] = json!({"type": "tool", "name": "Bash"});
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Bash"]);
        assert_eq!(report.pinned, ["Bash"]);
    }

    /// `any` names no tool, so there is nothing to protect — but it does
    /// require *something* to remain, which means a policy that empties
    /// `tools[]` breaks it however carefully this one is written.
    #[test]
    fn an_unnamed_tool_choice_protects_nothing() {
        let policy = Policy {
            mode: Mode::None,
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["tool_choice"] = json!({"type": "any"});
        apply(&mut body, &policy, true);
        assert!(names(&body).is_empty(), "an unnamed choice pins nothing");
    }

    #[test]
    fn mode_none_keeps_only_the_referenced_set() {
        let policy = Policy {
            mode: Mode::None,
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit"]);
        body["messages"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}
            ]},
        ]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Edit"], "only what was called");
        assert_eq!(report.pinned, ["Edit"]);
        let mut removed = report.removed;
        removed.sort();
        assert_eq!(removed, ["Bash", "Read"]);
    }

    /// `ToolSearch` is exempt in every mode — without it the client cannot
    /// reach any tool that was not already in the array.
    #[test]
    fn tool_search_survives_every_mode() {
        for mode in [Mode::None, Mode::Referenced, Mode::Allow] {
            let policy = Policy {
                mode,
                ..Policy::default()
            };
            let mut body = request(&["ToolSearch", "Read"]);
            body["messages"] = json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
            ]);
            apply(&mut body, &policy, true);
            assert!(
                names(&body).contains(&"ToolSearch".to_owned()),
                "{mode:?} dropped ToolSearch"
            );
        }
    }

    /// First request in a conversation: nothing has been called yet, so
    /// pruning would leave the model with nothing to work with.
    #[test]
    fn mode_referenced_sends_everything_on_the_first_turn() {
        let policy = Policy {
            mode: Mode::Referenced,
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Bash", "Edit"]);
        assert!(report.removed.is_empty());
        assert_eq!(report.offered, 3);
        assert_eq!(report.sent, 3, "the report must not read as pruned");
    }

    #[test]
    fn mode_referenced_prunes_once_the_conversation_has_answered() {
        let policy = Policy {
            mode: Mode::Referenced,
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit"]);
        body["messages"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            ]},
        ]);
        apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read"]);
    }

    #[test]
    fn mode_allow_sends_only_the_list_in_list_order() {
        let policy = Policy {
            mode: Mode::Allow,
            allow: vec!["Edit".into(), "Read".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash", "Edit", "Grep"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Edit", "Read"], "allow order, not wire order");
        assert_eq!(report.sent, 2);
    }

    /// `allow` is a filter, but a pinned name is still protected: the two
    /// rules are not in a precedence fight, both are satisfied.
    #[test]
    fn mode_allow_keeps_a_referenced_tool_the_list_omits() {
        let policy = Policy {
            mode: Mode::Allow,
            allow: vec!["Read".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["messages"] = json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
            ]},
        ]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Bash"]);
        assert_eq!(report.pinned, ["Bash"]);
    }

    #[test]
    fn add_appends_a_definition_the_client_did_not_send() {
        let policy = Policy {
            add: vec![json!({"name": "ask_user",
                             "description": "ask",
                             "input_schema": {"type": "object"}})],
            ..Policy::default()
        };
        let mut body = request(&["Read"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "ask_user"], "appended, not prepended");
        assert_eq!(report.added, 1);
        assert!(report.refused.is_empty());
    }

    #[test]
    fn add_refuses_to_shadow_a_tool_the_client_sent() {
        let policy = Policy {
            add: vec![json!({"name": "Read", "description": "mine",
                             "input_schema": {"type": "object"}})],
            ..Policy::default()
        };
        let mut body = request(&["Read"]);
        let before = body.clone();
        let report = apply(&mut body, &policy, true);
        assert_eq!(body, before, "a refused add changes nothing");
        assert_eq!(report.refused, [Refusal::Shadow("Read".into())]);
        assert_eq!(report.added, 0);
    }

    #[test]
    fn add_refuses_a_nameless_definition() {
        let policy = Policy {
            add: vec![json!({"description": "no name"})],
            ..Policy::default()
        };
        let mut body = request(&["Read"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(report.refused, [Refusal::Nameless]);
        assert_eq!(names(&body), ["Read"]);
    }

    #[test]
    fn add_refuses_a_duplicate_within_its_own_list() {
        let policy = Policy {
            add: vec![
                json!({"name": "twice", "input_schema": {"type": "object"}}),
                json!({"name": "twice", "input_schema": {"type": "object"}}),
            ],
            ..Policy::default()
        };
        let mut body = request(&["Read"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "twice"]);
        assert_eq!(report.added, 1);
        assert_eq!(report.refused, [Refusal::Shadow("twice".into())]);
    }

    /// Injection does not require an existing array — that is the whole
    /// point of `add`, since a disabled-everything client sends none.
    #[test]
    fn add_creates_tools_when_the_client_sent_none() {
        let policy = Policy {
            mode: Mode::None,
            add: vec![json!({"name": "solo", "input_schema": {"type": "object"}})],
            ..Policy::default()
        };
        let mut body = json!({"model": "m", "messages": []});
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["solo"]);
        assert_eq!(report.added, 1);
    }

    fn described(name: &str, description: &str) -> Value {
        json!({"name": name, "description": description, "input_schema": {"type": "object"}})
    }

    fn description_of(body: &Value, name: &str) -> String {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| tool_name(t) == Some(name))
            .and_then(|t| t.get("description"))
            .and_then(Value::as_str)
            .unwrap()
            .to_owned()
    }

    fn rewritten(rules: [(&str, Normalise); 1]) -> BTreeMap<String, Vec<Normalise>> {
        rules
            .into_iter()
            .map(|(name, rule)| (name.to_owned(), vec![rule]))
            .collect()
    }

    /// The case the verb exists for: a sentence naming the current date is
    /// stable for at most a day, so leaving it in defeats the prompt cache.
    #[test]
    fn dates_strip_the_volatile_sentence_and_keep_the_instruction() {
        let policy = Policy {
            rewrite: rewritten([("Search", Normalise::Dates)]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [described(
            "Search",
            "Search the web. \
             The current date is September 10, 2026."
        )]});
        let report = apply(&mut body, &policy, true);
        assert_eq!(description_of(&body, "Search"), "Search the web.");
        assert_eq!(report.rewritten, ["Search"]);
    }

    #[test]
    fn provider_strips_the_locality_claim() {
        let policy = Policy {
            rewrite: rewritten([("Web", Normalise::Provider)]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [described(
            "Web",
            "Fetch a page. This tool is US based."
        )]});
        apply(&mut body, &policy, true);
        assert_eq!(description_of(&body, "Web"), "Fetch a page.");
    }

    /// The blunt rule, and the only one an operator can point at a phrase of
    /// their own choosing.
    #[test]
    fn replaced_swaps_a_phrase() {
        let policy = Policy {
            rewrite: rewritten([(
                "Bash",
                Normalise::Replaced {
                    find: "the shell".into(),
                    replace: "a shell".into(),
                },
            )]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [described("Bash", "Run a command in the shell.")]});
        let report = apply(&mut body, &policy, true);
        assert_eq!(description_of(&body, "Bash"), "Run a command in a shell.");
        assert_eq!(report.rewritten, ["Bash"]);
    }

    #[test]
    fn replaced_can_delete_a_phrase_outright() {
        let policy = Policy {
            rewrite: rewritten([(
                "Bash",
                Normalise::Replaced {
                    find: " very".into(),
                    replace: String::new(),
                },
            )]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [described("Bash", "Run a very long command.")]});
        apply(&mut body, &policy, true);
        assert_eq!(description_of(&body, "Bash"), "Run a long command.");
    }

    /// Normalising prose must never move the tool set: a rewrite that pruned a
    /// tool would break a conversation already mid-flight, which is the whole
    /// reason this verb was split out from `disable`.
    #[test]
    fn rewriting_prose_never_moves_a_tool() {
        let policy = Policy {
            rewrite: BTreeMap::from([
                ("Bash".to_owned(), vec![Normalise::Dates]),
                ("Gone".to_owned(), vec![Normalise::Provider]),
            ]),
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        let report = apply(&mut body, &policy, true);
        assert_eq!(names(&body), ["Read", "Bash"]);
        assert!(report.removed.is_empty());
        assert_eq!(report.sent, 2);
    }

    #[test]
    fn a_tool_with_no_description_is_left_alone() {
        let policy = Policy {
            rewrite: rewritten([("Bash", Normalise::Dates)]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [{"name": "Bash", "input_schema": {"type": "object"}}]});
        let before = body.clone();
        let report = apply(&mut body, &policy, true);
        assert_eq!(body, before);
        assert!(report.rewritten.is_empty(), "nothing changed, so nothing is logged");
    }

    #[test]
    fn a_rule_for_a_tool_that_was_not_sent_is_inert() {
        let policy = Policy {
            rewrite: rewritten([("Absent", Normalise::Dates)]),
            ..Policy::default()
        };
        let mut body = request(&["Read"]);
        let before = body.clone();
        apply(&mut body, &policy, true);
        assert_eq!(body, before);
    }

    /// `changed_before_cache` decides whether to strip the client's cache
    /// marks, and the reason to strip is that the array was *rebuilt*: a mark
    /// on the third definition no longer points at the third definition. A
    /// rewrite edits a description in place and moves nothing, so the marks
    /// still point where the client put them and must be left alone. The
    /// content change still counts for `changed()`, which is what puts the
    /// pass in the log.
    #[test]
    fn a_rewrite_edits_in_place_and_leaves_the_cache_marks_where_they_were() {
        let policy = Policy {
            rewrite: rewritten([("Bash", Normalise::Dates)]),
            ..Policy::default()
        };
        let mut body = json!({"tools": [described("Bash", "Run it. The current date is May 1, 2026.")]});
        let report = apply(&mut body, &policy, true);
        assert!(report.changed(), "the description moved, so the pass is worth logging");
        assert!(
            !report.changed_before_cache(),
            "nothing was rebuilt, so the client's marks are still on the right tools"
        );
    }

    #[test]
    fn a_policy_that_only_rewrites_is_not_a_noop() {
        let policy = Policy {
            rewrite: rewritten([("Bash", Normalise::Dates)]),
            ..Policy::default()
        };
        assert!(!is_noop(&policy));
    }

    #[test]
    fn a_body_with_no_tools_and_nothing_to_add_is_untouched() {
        let policy = Policy {
            mode: Mode::None,
            ..Policy::default()
        };
        let mut body = json!({"model": "m", "messages": []});
        let before = body.clone();
        apply(&mut body, &policy, true);
        assert_eq!(body, before);
    }

    /// Rebuilding the array moves the client's cache breakpoints, so when
    /// the provider cannot hold them the marks go — a cold cache is better
    /// than a refused request.
    #[test]
    fn cache_marks_are_stripped_when_the_provider_cannot_take_them() {
        let policy = Policy {
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["tools"][0]["cache_control"] = json!({"type": "ephemeral"});
        let report = apply(&mut body, &policy, false);
        assert!(report.cache_stripped);
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_marks_are_left_alone_when_the_provider_takes_them() {
        let policy = Policy {
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["tools"][0]["cache_control"] = json!({"type": "ephemeral"});
        let report = apply(&mut body, &policy, true);
        assert!(!report.cache_stripped);
        assert!(body["tools"][0].get("cache_control").is_some());
    }

    /// `messages[]` carries the same marks and they are not ours to remove:
    /// stripping them would invalidate caches the tool policy never touched.
    #[test]
    fn only_tools_lose_their_cache_marks_never_messages() {
        let policy = Policy {
            disable: vec!["Bash".into()],
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        body["messages"][0]["cache_control"] = json!({"type": "ephemeral"});
        apply(&mut body, &policy, false);
        assert!(body["messages"][0].get("cache_control").is_some());
    }

    #[test]
    fn references_outside_the_window_are_not_pinned() {
        let policy = Policy {
            mode: Mode::None,
            ..Policy::default()
        };
        let mut body = request(&["Read", "Bash"]);
        let mut messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "old", "name": "Bash", "input": {}}
        ]})];
        for i in 0..PINS_WINDOW {
            messages.push(json!({"role": "user", "content": format!("filler {i}")}));
        }
        body["messages"] = Value::Array(messages);
        apply(&mut body, &policy, true);
        assert!(!names(&body).contains(&"Bash".to_owned()), "fell out of the window");
    }

    #[test]
    fn a_nameless_client_definition_is_forwarded_unchanged() {
        let policy = Policy {
            mode: Mode::None,
            ..Policy::default()
        };
        let mut body = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "hi"}],
            "tools": [{"description": "no name"}, tool("Read")],
        });
        let report = apply(&mut body, &policy, true);
        // `names()` filters nameless entries, so count the array instead:
        // the unnamed definition is forwarded and `Read` is not.
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(report.removed, ["Read"]);
        assert_eq!(report.sent, 1);
    }

    #[test]
    fn a_malformed_policy_field_is_a_hard_error() {
        let Err(err) = serde_json::from_value::<Policy>(json!({"mode": "sometimes"})) else {
            panic!("an unknown mode must not load");
        };
        assert!(err.to_string().contains("sometimes"));
    }

    #[test]
    fn a_policy_loads_from_an_empty_object_as_the_noop() {
        let policy: Policy = serde_json::from_value(json!({})).expect("defaults");
        assert_eq!(policy, Policy::default());
        assert!(is_noop(&policy));
    }

    /// An account file written before this field existed has no `tools` key
    /// at all; it must load as the no-op rather than failing.
    #[test]
    fn a_policy_round_trips() {
        let policy = Policy {
            mode: Mode::Allow,
            disable: vec!["Bash".into()],
            allow: vec!["Read".into()],
            add: vec![tool("extra")],
            rewrite: BTreeMap::from([("WebSearch".to_owned(), vec![Normalise::Provider])]),
        };
        let json = serde_json::to_value(&policy).expect("serialize");
        let back: Policy = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, policy);
    }

    #[test]
    fn every_mode_has_a_name_and_none_is_the_odd_one_out() {
        assert_eq!(Mode::default(), Mode::All);
        assert_eq!(Mode::All.as_str(), "all");
        assert_eq!(Mode::Referenced.as_str(), "referenced");
        assert_eq!(Mode::Allow.as_str(), "allow");
        assert_eq!(Mode::None.as_str(), "none");
    }

    /// The policy shipped in `docs/cloudflare-ips.policy.json` is a real
    /// artifact — an operator copies it into `POST /api/users/<id>/tools`.
    /// Parsing it here is what stops it rotting into a 400.
    #[test]
    fn the_shipped_cloudflare_policy_is_accepted() {
        let policy: Policy = serde_json::from_str(include_str!("../../../docs/cloudflare-ips.policy.json"))
            .expect("the shipped policy must deserialize");
        assert_eq!(policy.mode, Mode::All);
        assert_eq!(policy.add.len(), 1);
        validate(&policy).expect("the shipped policy must validate");

        // The name has to be one the proxy answers itself. If it is not, the
        // declaration is forwarded to a provider that has never heard of it,
        // which is the 400 this whole feature exists to avoid.
        let name = tool_name(&policy.add[0]).expect("the added tool carries a name");
        assert!(crate::localtool::is_local(name), "{name} is not a local tool");
    }
}
