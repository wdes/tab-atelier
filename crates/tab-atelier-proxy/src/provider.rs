// SPDX-License-Identifier: MPL-2.0

//! Where a request can be sent, and what lives there.
//!
//! The proxy chooses the model. A caller asks for one, but that name is a
//! statement about the KIND of work — something cheap and quick, something
//! that has to think — not an instruction about which endpoint to bill. The
//! caller cannot know which provider has capacity right now; the proxy can.
//!
//! So a provider is a place to send Anthropic-shaped traffic, and a model is
//! an entry in a [`Class`]. Routing picks a `(provider, model)` pair from the
//! class ([`crate::routing`]).
//!
//! # Why every provider here speaks the Anthropic wire format
//!
//! Claude Code speaks the Messages API, so that is the contract on the way in
//! and cannot change. Providers that speak the same shape — Anthropic itself,
//! the same models on Bedrock or Vertex, and the several third parties that
//! ship an Anthropic-compatible endpoint precisely to serve Claude Code — can
//! be swapped by changing a base URL, a credential and a model name. Nothing
//! is translated, so nothing is lost: tool use, prompt caching and extended
//! thinking all pass through untouched.
//!
//! That restriction is what makes rerouting *safe* rather than merely
//! possible. An OpenAI-shaped provider would need the request AND the
//! streamed response rewritten, and tool-call semantics do not survive that
//! trip intact — which for Claude Code, whose every turn is tool use, means a
//! reroute that silently breaks the client. When that adapter is written it
//! belongs behind its own [`Wire`] variant, and this comment is the reason it
//! is not simply "another base URL".
//!
//! # The capacity point
//!
//! Bedrock and Vertex serve the SAME models from DIFFERENT quota pools. That
//! is the whole reroute story: when the subscription's five-hour window is
//! spent, the work does not have to stop or get worse, it moves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The host part of a URL, lowercased.
///
/// Enough parsing for the one comparison that matters — which host a
/// credential is about to be sent to — and deliberately not a URL parser: this
/// crate has no business growing one to answer a two-line question.
#[must_use]
pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// How many mapping hops to follow before giving up.
///
/// A chain longer than this is not a routing strategy, it is a mistake — and
/// the loop has to terminate whatever the table says.
const MAX_MAPPING_HOPS: usize = 8;

/// What a model is for, rather than what it is called.
///
/// Routing happens within a class: a request that arrived asking for Opus is a
/// request for something in [`Class::Heavy`], and any heavy model with
/// capacity can serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Cheap and quick. Classification, formatting, short answers.
    Fast,
    /// The default working tier.
    Balanced,
    /// Slow, expensive, and better at hard problems.
    Heavy,
}

impl Class {
    /// Classes from cheapest up, so a search can walk them in order.
    pub const LADDER: [Self; 3] = [Self::Fast, Self::Balanced, Self::Heavy];

    /// The next class down, for when nothing in this one has capacity.
    #[must_use]
    pub const fn cheaper(self) -> Option<Self> {
        match self {
            Self::Heavy => Some(Self::Balanced),
            Self::Balanced => Some(Self::Fast),
            Self::Fast => None,
        }
    }
}

/// The request/response shape a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// The Anthropic Messages API — passed through untouched.
    #[default]
    Anthropic,
    /// `OpenAI`'s Chat Completions API, translated at the egress boundary.
    ///
    /// The translation lives in `openai.rs` and runs inside `forward()`, so
    /// everything upstream — shaping, the tool policy, the usage sniffer — is
    /// still looking at a Messages request and a Messages stream. This variant
    /// is the first thing in production to branch on `Wire`; until now it was
    /// persisted and tagged but read nowhere.
    Openai,
}

/// How to authenticate to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    /// The proxy host's own Claude login, refreshed as needed. No API key
    /// anywhere, which is why the subscription can be shared at all.
    ClaudeOauth,
    /// A key read from the environment at startup. Named rather than inlined:
    /// a credential in a config file is a credential in a backup.
    ApiKeyEnv { var: String },
    /// A key in its own file, `0600`.
    ///
    /// This is what makes a provider addable from the web UI. `ApiKeyEnv`
    /// cannot be: the service has already started, its environment is fixed,
    /// and `set_var` is unsafe. The alternative — inlining the key in
    /// `providers.json` — would put a live credential in the file an operator
    /// is most likely to copy around or paste into a bug report, which is the
    /// exact thing `ApiKeyEnv`'s comment above refuses to do.
    ///
    /// So the key gets its own file, written by the UI, readable only by the
    /// service account. `providers.json` records where, and never what.
    ApiKeyFile { path: String },
}

impl Auth {
    /// Whether a key can be set for this provider from the UI or the API.
    ///
    /// Only a file-backed credential can. The other two have a credential
    /// already, and it does not live here: writing a key file for a
    /// `claude_oauth` provider would be a write no code path reads, leaving a
    /// live Anthropic key on disk that nothing consults — and that an operator
    /// who later changed the auth kind would find suddenly in use.
    #[must_use]
    pub const fn accepts_a_key(&self) -> bool {
        matches!(self, Self::ApiKeyFile { .. })
    }

    /// Why a key cannot be set, phrased so the reader knows what to do instead.
    ///
    /// `None` when it can be.
    #[must_use]
    pub fn no_key_reason(&self, id: &str) -> Option<String> {
        // Single lines each: a `\`-continued Rust literal is tempting here and
        // the indentation of the next line ends up inside the message.
        match self {
            Self::ApiKeyFile { .. } => None,
            Self::ClaudeOauth => Some(format!(
                "provider {id} uses the proxy host's own Claude login — there is no key to set. Re-import it with `tab-atelier-proxy import-credentials`."
            )),
            Self::ApiKeyEnv { var } => Some(format!(
                "provider {id} reads its key from ${var} — set that in the service's environment and restart it. Writing a file here would be ignored."
            )),
        }
    }

    /// The credential, or a message saying what is missing.
    ///
    /// Read per request rather than captured at startup, so rotating a key is
    /// a file write and not a restart.
    ///
    /// # Errors
    /// The environment variable is unset or empty, or the file cannot be read
    /// or is empty.
    pub fn secret_with(&self, get: impl Fn(&str) -> Option<String>) -> Result<String, String> {
        match self {
            Self::ClaudeOauth => Err("claude_oauth is resolved by the egress, not here".to_owned()),
            Self::ApiKeyEnv { var } => get(var)
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| format!("no credential in ${var}")),
            Self::ApiKeyFile { path } => {
                let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
                let key = raw.trim().to_owned();
                if key.is_empty() {
                    return Err(format!("{path} is empty"));
                }
                Ok(key)
            }
        }
    }
}

/// What a model costs, in micro-USD per 1M tokens.
///
/// Micro-USD because the figures are small enough that whole dollars lose
/// them: `deepseek-flash` caches at $0.003/1M, so a busy hour is a fraction of
/// a cent and an integer of dollars would round a day's spend to zero.
///
/// **Indicative, and deliberately separate from [`Model::relative_cost`].**
/// Routing needs an ordering, which stays true for years; a bill needs an
/// absolute figure, which provably does not — that is why `relative_cost`
/// refuses to carry one and keeps its published rates in the comment on the
/// [`Preset::Deepseek`] arm below. This is the other half of that split: it
/// exists so an operator can see roughly what a conversation cost, and no
/// routing decision ever reads it.
///
/// **A missing price is not a zero price.** Only models whose rates are
/// actually recorded get one; see [`Registry::billing_price`] on what that
/// means for the ones that do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    /// Cached input, per 1M tokens.
    pub cache_hit: u32,
    /// Uncached input, per 1M tokens.
    ///
    /// On this scale it usually equals `relative_cost * 10_000`, because that
    /// is cents-per-million against micro-USD-per-million. The two are not
    /// tied together in code — a rate that moves must be edited in both places,
    /// and the cross-check test says so — but they have the same source.
    pub input: u32,
    /// Generated tokens, per 1M.
    pub output: u32,
}

impl Price {
    /// The same rates at a peak multiplier, rounded down.
    ///
    /// Mirrors [`Provider::cost_at`], which scales `relative_cost` identically.
    /// Rounding down rather than to-nearest so a doubled rate cannot land a
    /// micro-USD above the figure the provider publishes.
    #[must_use]
    const fn scaled(self, percent: u32) -> Self {
        Self {
            cache_hit: self.cache_hit.saturating_mul(percent) / 100,
            input: self.input.saturating_mul(percent) / 100,
            output: self.output.saturating_mul(percent) / 100,
        }
    }

    /// What a mixed batch of tokens costs, in micro-USD.
    ///
    /// The three classes are arguments rather than one total on purpose: they
    /// are priced differently — 50x apart on Flash — so a caller that summed
    /// them first would already have lost the answer. `i128` accumulator, like
    /// the rest of the money arithmetic here, because a busy window times a
    /// doubled peak rate overflows `i64` well before it stops being plausible.
    #[must_use]
    pub fn cost_micro(&self, cache_read: u64, input: u64, output: u64) -> i128 {
        let (input_side, output_side) = self.split_micro(cache_read, input, output);
        input_side + output_side
    }

    /// The same cost, kept in the two halves the charts draw.
    ///
    /// Defined once and summed by [`Self::cost_micro`] rather than written
    /// twice: the chart's two panels and the totals tile must agree, and the
    /// way to guarantee that is for the total to be the sum of the parts
    /// rather than a second calculation that is expected to match.
    #[must_use]
    pub fn split_micro(&self, cache_read: u64, input: u64, output: u64) -> (i128, i128) {
        let hit = i128::from(self.cache_hit) * i128::from(cache_read);
        let miss = i128::from(self.input) * i128::from(input);
        let out = i128::from(self.output) * i128::from(output);
        ((hit + miss) / 1_000_000, out / 1_000_000)
    }
}

/// A rate with a provider's peak multiplier applied when it is in force.
///
/// One implementation, used by both the routing-facing [`Provider::cost_at`]
/// and the billing-facing [`Rate::at`], so a change to how peak is decided
/// cannot land in one and miss the other.
fn price_with_peak(price: Price, peak: Option<&Peak>, unix_secs: u64) -> Price {
    match peak {
        Some(p) if p.active_at(unix_secs) => price.scaled(p.multiplier_percent),
        _ => price,
    }
}

/// What an account's tokens cost, and the rate that answer came from.
///
/// Owned rather than borrowed because the registry is behind a lock: a borrow
/// cannot outlive the guard, and the aggregation spending this runs long after
/// it is released.
///
/// [`Self::price`] is deliberately the **off-peak** rate. Peak is applied per
/// hour by [`Self::at`], never here, so that an hour billed at double is priced
/// at double and the hours around it are not — pricing a whole window from one
/// reading of the clock would get both halves wrong on either side of a
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rate {
    /// Published rates per 1M tokens, in micro-USD, off-peak.
    pub price: Price,
    /// The provider's peak schedule, if it has one.
    pub peak: Option<Peak>,
    /// The model these rates belong to, for the UI's provenance note.
    pub model: String,
}

impl Rate {
    /// This rate at an instant, peak included.
    #[must_use]
    pub fn at(&self, unix_secs: u64) -> Price {
        price_with_peak(self.price, self.peak.as_ref(), unix_secs)
    }
}

/// One model a provider serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// The id to send upstream. Provider-specific — the same model is
    /// `claude-opus-5` at Anthropic and something longer on Bedrock.
    pub id: String,
    pub class: Class,
    /// Relative price, only ever compared with other entries. An absolute
    /// figure would be wrong within a month; the ORDER is what routing needs,
    /// and that is stable.
    #[serde(default = "one")]
    pub relative_cost: u32,
    /// What this costs, when the rates are known. See [`Price`].
    ///
    /// Absent for most models, and deliberately so. The subscription hop has no
    /// per-token price to state — it is a flat plan, and a metered figure for
    /// it would invent a marginal cost that does not exist — and no other hop's
    /// published rates are recorded in this repository yet. `None` means "show
    /// no figure", never "costs nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// Withdrawn or about to be. Routing skips it; the UI shows why.
    ///
    /// A deprecated model is worse than a missing one, because it still
    /// accepts traffic. `deepseek-v4-pro` is withdrawn on 2026-09-14 and
    /// requests to it are silently served by a different model at a different
    /// price — a router that kept offering it would be reporting a cost and a
    /// capability that are both about to become untrue.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
    /// Why, for the operator. Shown beside the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

const fn one() -> u32 {
    1
}

impl Model {
    /// A plain, current model.
    #[must_use]
    pub fn new(id: &str, class: Class, relative_cost: u32) -> Self {
        Self {
            id: id.to_owned(),
            class,
            relative_cost,
            price: None,
            deprecated: false,
            note: None,
        }
    }

    /// The same model, with what it costs where those rates are known.
    ///
    /// Chained onto [`Self::new`] rather than added as arguments, so that the
    /// many models with no recorded rates keep the short constructor and
    /// nothing has to pass a placeholder to say "unknown".
    #[must_use]
    pub fn priced(self, cache_hit: u32, input: u32, output: u32) -> Self {
        Self {
            price: Some(Price {
                cache_hit,
                input,
                output,
            }),
            ..self
        }
    }

    /// A model being withdrawn, with the reason.
    #[must_use]
    pub fn retiring(id: &str, class: Class, relative_cost: u32, note: &str) -> Self {
        Self {
            deprecated: true,
            note: Some(note.to_owned()),
            ..Self::new(id, class, relative_cost)
        }
    }
}

/// A stretch of the week a provider charges more for the same tokens.
///
/// Not decoration. `relative_cost` exists to compare providers, and a provider
/// whose price doubles for seven hours of every weekday is a genuinely
/// different deal during those hours — `DeepSeek`'s off-peak input is half its
/// peak input, which is enough to reorder a preference list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peak {
    /// Percentage of the off-peak price charged during these windows.
    /// 200 = double.
    pub multiplier_percent: u32,
    pub windows: Vec<PeakWindow>,
}

/// When peak applies, in UTC.
///
/// Everything here is UTC because the provider states its own schedule that
/// way, and a local-time copy would be wrong twice a year in exactly the way
/// nobody notices until a bill arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeakWindow {
    /// ISO weekday numbers, 1 = Monday .. 7 = Sunday.
    pub weekdays: Vec<u8>,
    /// Hour of day, UTC, half-open: `[start_hour, end_hour)`.
    pub start_hour: u8,
    pub end_hour: u8,
}

impl Peak {
    /// Whether peak pricing is in force at this instant.
    #[must_use]
    pub fn active_at(&self, unix_secs: u64) -> bool {
        let Ok(ts) = i64::try_from(unix_secs).map(jiff::Timestamp::from_second) else {
            return false;
        };
        let Ok(ts) = ts else { return false };
        let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
        // Monday is 1 .. Sunday is 7, matching `PeakWindow::weekdays`.
        let weekday = zoned.weekday().to_monday_one_offset();
        let hour = zoned.hour();
        self.windows.iter().any(|w| {
            u8::try_from(weekday).is_ok_and(|d| w.weekdays.contains(&d))
                && hour >= i8::try_from(w.start_hour).unwrap_or(24)
                && hour < i8::try_from(w.end_hour).unwrap_or(0)
        })
    }

    /// When the peak in force right now ends, as a Unix second.
    ///
    /// `None` when no window is active. Windows are hour-granular and UTC, so
    /// the search walks the hour boundary rather than the request instant: the
    /// answer is exact, and it crosses midnight and weekday changes without
    /// special-casing them. A schedule that somehow never clears reports `None`
    /// ("in force", no end) rather than inventing one.
    #[must_use]
    pub fn active_until(&self, unix_secs: u64) -> Option<u64> {
        if !self.active_at(unix_secs) {
            return None;
        }
        let mut at = (unix_secs / 3_600 + 1) * 3_600;
        for _ in 0..(24 * 8) {
            if !self.active_at(at) {
                return Some(at);
            }
            at += 3_600;
        }
        None
    }
}

/// Somewhere requests can go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub wire: Wire,
    /// No trailing slash. `/v1/messages` is appended.
    pub base_url: String,
    pub auth: Auth,
    pub models: Vec<Model>,
    /// Tried in ascending order when several can serve a class. The
    /// subscription is 0 because it is already paid for.
    #[serde(default)]
    pub preference: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// When this provider charges more for the same tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak: Option<Peak>,
}

impl Provider {
    /// What this model costs right now, in `relative_cost` units.
    ///
    /// The peak multiplier is applied here rather than baked into
    /// `relative_cost`, so one number in the file stays true all week and the
    /// schedule explains itself.
    #[must_use]
    pub fn cost_at(&self, model: &Model, unix_secs: u64) -> u32 {
        match &self.peak {
            Some(p) if p.active_at(unix_secs) => model.relative_cost.saturating_mul(p.multiplier_percent) / 100,
            _ => model.relative_cost,
        }
    }

    /// Whether this provider serves a model by that exact id.
    #[must_use]
    pub fn serves(&self, model_id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == model_id && !m.deprecated)
    }

    /// What this model's tokens cost right now, in micro-USD per 1M.
    ///
    /// The same peak test [`Self::cost_at`] applies to `relative_cost`, applied
    /// to the published rates. Two functions rather than one because they
    /// answer to different masters: `cost_at` orders candidates for routing and
    /// must never move, while this one is a bill and moves whenever a provider
    /// republishes. `None` when the rates are not recorded — see [`Price`].
    #[must_use]
    pub fn price_at(&self, model: &Model, unix_secs: u64) -> Option<Price> {
        Some(price_with_peak(model.price?, self.peak.as_ref(), unix_secs))
    }
}

/// "When someone asks for X, use Y."
///
/// Two jobs, and they are the same mechanism:
///
/// * **Across providers.** Claude Code asks for `claude-opus-5`; `DeepSeek` has
///   never heard of it. `DeepSeek`'s own endpoint papers over this by mapping
///   `claude-opus*` to `deepseek-v4-pro` server-side — which is a decision
///   about *their* margin and *their* roadmap, made where the operator cannot
///   see or change it. Ours is visible, editable, and in one place.
/// * **Within one provider.** `claude-opus-5 → claude-sonnet-5` is a
///   deliberate downgrade: the same provider, a cheaper class, chosen by the
///   operator rather than forced by capacity.
///
/// A mapping rewrites the NAME. Everything downstream — health, preference,
/// the degrade ladder — still applies, so a mapping cannot quietly become a
/// hard pin that survives an outage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// The name a client asks for. Matched exactly, then case-insensitively.
    pub from: String,
    /// The name to use instead.
    pub to: String,
    /// For the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

const fn yes() -> bool {
    true
}

impl Provider {
    /// The cheapest current model this provider has in a class, if any.
    ///
    /// Cheapest *right now*: a provider on peak pricing is a different deal
    /// for those hours, and `deepseek-flash` doubling is exactly the kind of
    /// thing that should flip a close call. Deprecated models are never
    /// offered — see [`Model::deprecated`].
    #[must_use]
    pub fn model_for(&self, class: Class, now: u64) -> Option<&Model> {
        self.models
            .iter()
            .filter(|m| m.class == class && !m.deprecated)
            .min_by_key(|m| self.cost_at(m, now))
    }

    /// Whether peak pricing is in force right now, for the UI to say so.
    #[must_use]
    pub fn peak_now(&self, now: u64) -> bool {
        self.peak.as_ref().is_some_and(|p| p.active_at(now))
    }

    /// When the peak in force right now ends, so the UI can say "until HH:MM"
    /// rather than only that peak is on.
    #[must_use]
    pub fn peak_until(&self, now: u64) -> Option<u64> {
        self.peak.as_ref().and_then(|p| p.active_until(now))
    }

    /// Whether sending to this provider spends the shared Claude subscription.
    ///
    /// True exactly when the credential is the host's own Claude login, which
    /// is also what [`Provider::unusable_reason`] ties to Anthropic's host —
    /// so "spends the plan" and "is the subscription" cannot drift apart.
    ///
    /// This is the line the `QoS` scheduler is drawn along. Its budget is read
    /// from `anthropic-ratelimit-*` headers and from the plan monitor, so it
    /// measures ONE quota; a request bound for a provider that bills
    /// separately is not spending it and must not be gated by it. Getting this
    /// wrong is not subtle in effect — a saturated subscription stops traffic
    /// it has nothing to do with, and the far end that could have served it
    /// sits idle.
    #[must_use]
    pub const fn uses_the_subscription(&self) -> bool {
        matches!(self.auth, Auth::ClaudeOauth)
    }

    /// Why compaction must not be enabled on this provider, if it must not be.
    ///
    /// `claude_oauth` means the subscription hop, and compaction there is
    /// self-defeating rather than merely useless. `cache_control` breakpoints
    /// make everything before the last one a cache read at roughly a tenth of
    /// list price; a body rewritten in the middle of `messages` invalidates
    /// every breakpoint after the edit, so the next turn is a full-price miss.
    /// A shorter body and a bigger bill is a configuration that cannot be
    /// correct, and a setting that cannot be correct should not be offerable —
    /// enforced here so the API refuses it as well as the button.
    ///
    /// `None` when the level is acceptable, including `none` on any provider.
    #[must_use]
    pub fn compact_refusal(&self, wanted: crate::compact::Compact) -> Option<String> {
        if wanted.is_none() || !matches!(self.auth, Auth::ClaudeOauth) {
            return None;
        }
        Some(format!(
            "provider {} authenticates with the host's own Claude login, so compaction there only costs money: \
             rewriting the body invalidates every cache breakpoint after the edit and turns the next turn into a \
             full-price miss. Leave it on `none`, or use Anthropic's own context_management on a provider that \
             honours it.",
            self.id
        ))
    }

    /// Why this provider must never be used, if there is such a reason.
    ///
    /// A coherence check rather than a credential check, and it guards the one
    /// combination that leaks:
    ///
    /// `auth: claude_oauth` means "send the PROXY HOST'S OWN Claude access
    /// token". Point that at anything but Anthropic and the proxy hands its
    /// subscription credential to a third party on every request — silently,
    /// because the request succeeds and the far end is delighted. Nothing in
    /// the type system connects the auth kind to the URL, so the connection is
    /// made here and enforced wherever a provider is considered.
    #[must_use]
    pub fn unusable_reason(&self) -> Option<String> {
        match &self.auth {
            Auth::ClaudeOauth if host_of(&self.base_url) != host_of(crate::egress::ANTHROPIC_BASE) => Some(format!(
                "provider {} has auth claude_oauth but points at {}, not Anthropic — that would send this \
                 host's own Claude token to a third party. Use an api_key_file credential instead.",
                self.id,
                host_of(&self.base_url)
            )),
            _ => None,
        }
    }

    /// Whether its credential is actually present.
    ///
    /// A provider configured but unusable must be visibly unusable — routing
    /// to one whose key is missing produces a 401 from somewhere the operator
    /// was not looking. For the subscription the credential is this host's
    /// Claude file rather than a key, so that too is looked for here; it is the
    /// same question, asked of the only kind whose answer is a path.
    #[must_use]
    pub fn credential_ready(&self) -> bool {
        self.credential_ready_with(|v| std::env::var(v).ok(), crate::egress::credential_present())
    }

    /// The same question with the environment supplied.
    ///
    /// Tests use this rather than setting variables: `set_var` is `unsafe`
    /// (forbidden in this crate) and races every other test in the binary.
    ///
    /// `subscription_present` is injected for the same reason: whether the Claude
    /// credential file exists is host state, and reading it from the real `HOME`
    /// would make every routing test depend on the machine it happens to run on.
    #[must_use]
    pub fn credential_ready_with(&self, get: impl Fn(&str) -> Option<String>, subscription_present: bool) -> bool {
        if self.unusable_reason().is_some() {
            return false;
        }
        match &self.auth {
            // The subscription has no key to look up: its credential is the Claude
            // file, and a missing one is exactly as unusable as a missing key.
            Auth::ClaudeOauth => subscription_present,
            Auth::ApiKeyEnv { var } => get(var).is_some_and(|v| !v.trim().is_empty()),
            // A file is checked for existence, not read: this runs on every
            // routing decision and the contents are fetched per request.
            Auth::ApiKeyFile { path } => std::fs::metadata(path).is_ok_and(|m| m.len() > 0),
        }
    }

    /// Whether it may serve at all: the operator left it on, and it can
    /// authenticate.
    ///
    /// The one place the two ways of being disabled meet, so that routing and the
    /// dashboard cannot disagree about whether this provider is in play. A
    /// switched-off provider and one whose credential is missing are both, in the
    /// only sense a caller cares about, disabled.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.usable_with(|v| std::env::var(v).ok(), crate::egress::credential_present())
    }

    /// [`Provider::usable`] with the host state supplied, for the same reason as
    /// [`Provider::credential_ready_with`].
    #[must_use]
    pub fn usable_with(&self, get: impl Fn(&str) -> Option<String>, subscription_present: bool) -> bool {
        self.enabled && self.credential_ready_with(get, subscription_present)
    }

    /// Why it may not serve a request right now, or `None` if it may.
    ///
    /// The whole "is this provider in play" question with the reason kept, for
    /// the callers that have to explain themselves rather than quietly pick
    /// something else. [`Provider::usable`] is this same judgement without the
    /// words; they are written together so a refusal can never be reported for
    /// a provider that was a candidate, or the reverse.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        self.refusal_with(|v| std::env::var(v).ok(), crate::egress::credential_present())
    }

    /// [`Provider::refusal`] with the host state supplied, for the same reason as
    /// [`Provider::credential_ready_with`]: the words a refusal produces are
    /// user-facing and worth asserting on, and that cannot be done against
    /// whatever `HOME` the test happens to run under.
    #[must_use]
    pub fn refusal_with(&self, get: impl Fn(&str) -> Option<String>, subscription_present: bool) -> Option<String> {
        if !self.enabled {
            return Some(format!("provider {} is switched off", self.id));
        }
        if let Some(why) = self.unusable_reason() {
            return Some(why);
        }
        (!self.credential_ready_with(get, subscription_present)).then(|| self.missing_credential())
    }

    /// What a caller needs to fix a provider whose credential is not present.
    ///
    /// Names the exact place looked, because "no credential" is not actionable
    /// on its own — the file it wants is a path the operator set up once and
    /// has to recognise now. The subscription's is the host's own Claude login,
    /// which is where the read error a bare `true` used to produce came from.
    #[must_use]
    fn missing_credential(&self) -> String {
        match &self.auth {
            Auth::ClaudeOauth => format!(
                "provider {} spends this host's Claude plan, and its credential file is not there: {}",
                self.id,
                crate::egress::credentials_file()
                    .map_or_else(|_| "no home directory".to_owned(), |p| p.display().to_string())
            ),
            Auth::ApiKeyEnv { var } => format!("provider {} needs ${var}, which is not set", self.id),
            Auth::ApiKeyFile { path } => format!(
                "provider {} needs its key file {path}, which is missing or empty",
                self.id
            ),
        }
    }

    /// Put back the rates the catalogue publishes for models this row leaves
    /// unpriced.
    ///
    /// A price is a property of the vendor's list, not of an operator's edit,
    /// and nothing outside this file can set one: the provider form rebuilds
    /// every model from `id:class:relative_cost` text and [`parse_models`] has
    /// no field for a rate, so a single save drops the triple from the row. The
    /// same happens to a file written before the rates existed at all, because
    /// `price` is `#[serde(default)]` and simply deserialises to `None`. Either
    /// way the provider keeps working and keeps counting tokens, while every
    /// hour it serves draws no money — an empty money unit that looks like a
    /// broken chart rather than a missing figure.
    ///
    /// Filling from the catalogue turns that loss into a recoverable one. It can
    /// only restore what the catalogue itself declares, so a model deliberately
    /// left unpriced stays unpriced, and a rate set by hand is left alone.
    pub fn adopt_published_rates(&mut self) {
        for model in &mut self.models {
            if model.price.is_none() && !model.deprecated {
                model.price = published_price(&model.id);
            }
        }
    }

    /// Whether not one model it serves carries a rate.
    ///
    /// Not a fault on its own: the subscription hop is unpriced by design, since
    /// a flat plan has no per-token cost to state. It is worth saying out loud
    /// all the same, because the only visible symptom is a money figure that
    /// never appears.
    #[must_use]
    pub fn has_no_recorded_rate(&self) -> bool {
        !self.models.iter().any(|m| !m.deprecated && m.price.is_some())
    }
}

/// A provider an operator can add in one click, with its models and prices.
///
/// Kept in code rather than in the docs because a preset that is merely
/// *described* is a preset somebody has to retype, and the prices below are
/// the whole reason to use it.
///
/// Anthropic's own subscription is not a preset: it needs no credential to
/// type, and it is already the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Deepseek,
    Openai,
}

impl Preset {
    pub const ALL: [Self; 2] = [Self::Deepseek, Self::Openai];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Deepseek => "deepseek",
            Self::Openai => "openai",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Deepseek => "DeepSeek",
            Self::Openai => "OpenAI",
        }
    }

    /// The provider, ready to have a key written into it. `config_dir` is
    /// threaded in rather than looked up: the key path must be absolute, and
    /// a guess resolved against a working directory is a file the service
    /// writes and then cannot find.
    ///
    /// # Prices are `DeepSeek`'s, as published
    ///
    /// Off-peak, per 1M tokens: Flash $0.003 cache-hit input / $0.15 miss /
    /// $0.60 output; V4 Pro $0.022 / $0.66 / $1.98. `relative_cost` is only
    /// ever compared with other entries, so these are the cache-MISS input
    /// figures as integers against Anthropic Haiku at 100 — which is the
    /// number that actually decides a reroute.
    ///
    /// Peak is 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday, at double.
    #[must_use]
    pub fn provider(self, config_dir: &Path) -> Provider {
        self.provider_named(config_dir, self.id())
    }

    /// The next free provider for this preset among `taken`.
    ///
    /// Two keys for one provider coexist only if their ids differ — and the id
    /// is also what gives each key its own file on disk. Reusing a name would
    /// be an edit of the entry already there, silently replacing the key
    /// stored at that path.
    #[must_use]
    pub fn provider_for<'a>(self, config_dir: &Path, taken: impl IntoIterator<Item = &'a str>) -> Provider {
        let taken: Vec<&str> = taken.into_iter().collect();
        let base = self.id();
        let mut id = base.to_owned();
        let mut n = 1;
        while taken.iter().any(|t| *t == id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        self.provider_named(config_dir, &id)
    }

    /// The body of [`Preset::provider`], with the name supplied rather than
    /// derived from the preset.
    #[must_use]
    pub fn provider_named(self, config_dir: &Path, id: &str) -> Provider {
        match self {
            Self::Deepseek => Provider {
                id: id.to_owned(),
                wire: Wire::Anthropic,
                // The ANTHROPIC-shaped endpoint. DeepSeek also serves an
                // OpenAI-shaped one on the bare host; pointing at it would
                // produce a 404 or a silently mangled tool call per request.
                base_url: "https://api.deepseek.com/anthropic".to_owned(),
                auth: Auth::ApiKeyFile {
                    path: provider_key_path(config_dir, id).display().to_string(),
                },
                // After the subscription: the subscription is already paid
                // for, so it is only worth leaving when it is out of capacity.
                preference: 10,
                enabled: true,
                peak: Some(Peak {
                    multiplier_percent: 200,
                    windows: vec![
                        PeakWindow {
                            weekdays: vec![1, 2, 3, 4, 5],
                            start_hour: 1,
                            end_hour: 4,
                        },
                        PeakWindow {
                            weekdays: vec![1, 2, 3, 4, 5],
                            start_hour: 6,
                            end_hour: 10,
                        },
                    ],
                }),
                models: vec![
                    // 1M context, thinking and non-thinking, tool calls,
                    // vision. The only model here that is not being withdrawn.
                    //
                    // The one model in this file with a recorded rate triple,
                    // so the one the money unit can price. Published per 1M,
                    // as the comment above gives them: $0.003 cached input,
                    // $0.15 uncached, $0.60 output. Peak doubles all three.
                    Model::new("deepseek-flash", Class::Balanced, 15).priced(3_000, 150_000, 600_000),
                    // Withdrawn 2026-09-14, after which requests to it are
                    // served by Flash at Flash's price. Listed so an operator
                    // can see it existed and why it is not offered — not
                    // routed to, because it would report a cost and a
                    // capability that are both about to stop being true.
                    //
                    // This is the published table's second column: $0.022 hit
                    // / $0.66 miss / $1.98 out per 1M, doubling at peak to
                    // $0.044 / $1.32 / $3.96. That it is this model is not a
                    // guess — the miss rate is 4.40x Flash's, which is exactly
                    // what `relative_cost: 66` against Flash's 15 already
                    // encoded, arrived at independently.
                    //
                    // Worth knowing it is NOT a flat multiple of Flash: the
                    // hit rate is 7.33x and the output 3.30x, so the two
                    // models have different shapes — Flash 1:50:200 across
                    // hit:miss:out, this one 1:30:90. That is the whole reason
                    // `relative_cost` cannot price anything and a real triple
                    // is needed per model: one scalar describes one class, and
                    // scaling Flash's triple by this model's miss ratio would
                    // be wrong on two of the three.
                    //
                    // Still not wired into `price`, and now for a checkable
                    // reason rather than a guess: `billing_price` filters
                    // `!m.deprecated` on both its paths, so a price here could
                    // never be selected for billing. It would surface only in
                    // the providers panel, reading "$0.66/1M" for requests
                    // that actually bill at Flash's $0.15.
                    Model::retiring(
                        "deepseek-v4-pro",
                        Class::Heavy,
                        66,
                        "withdrawn 2026-09-14; requests are served by deepseek-flash at Flash prices",
                    ),
                ],
            },
            // GPT-5.6 ships as three models rather than one with a dial. They
            // are listed separately, by their real ids, so each can be pinned
            // per user with a plain model override — which is also what makes
            // a per-person default possible (see `Account::model`).
            //
            // # Prices are OpenAI's, as published
            //
            // Per 1M tokens, Sol $5 in / $30 out, Terra $2.50 / $15, Luna
            // $1 / $6. Same convention as `DeepSeek` above: cache-MISS input
            // as integers against Anthropic Haiku at 100, which is the figure
            // that actually decides a reroute. So Luna 100, Terra 250, Sol
            // 500 — and by output price Sol is dearer than Opus 5.
            //
            // Note the cache discount does NOT reach the proxy: OpenAI's
            // `prompt_tokens_details.cached_tokens` is surfaced to the client
            // in `cache_read_input_tokens`, but `relative_cost` models only
            // input and output, exactly as it does for every other provider.
            //
            // No `Price` on these three, though their input and output rates
            // are right here: OpenAI's cached-input rate is not recorded in
            // this repository, and billing cached tokens at the miss rate
            // would overstate the bill precisely on the traffic that is most
            // cached — the classifier reads at 99.8%. A wrong number is worse
            // than no number, so these show an unknown cost until that rate is
            // recorded alongside the other two.
            Self::Openai => Provider {
                id: id.to_owned(),
                wire: Wire::Openai,
                base_url: "https://api.openai.com/v1".to_owned(),
                auth: Auth::ApiKeyFile {
                    path: provider_key_path(config_dir, id).display().to_string(),
                },
                // The same reasoning as DeepSeek: the subscription is already
                // paid for, so a metered provider is only worth leaving to when
                // it is out of capacity.
                preference: 10,
                enabled: true,
                peak: None,
                models: vec![
                    Model::new("gpt-5.6-sol", Class::Heavy, 500),
                    Model::new("gpt-5.6-terra", Class::Balanced, 250),
                    Model::new("gpt-5.6-luna", Class::Fast, 100),
                ],
            },
        }
    }

    /// The models a preset ships, without configuring a whole provider.
    ///
    /// Delegates to [`Preset::provider_named`] so the rates written beside the
    /// model list stay the only copy of them: a second table here would be one
    /// more thing to leave behind when a vendor moves a price. The key path it
    /// builds on the way is discarded, and no directory is read or written.
    #[must_use]
    pub fn models(self) -> Vec<Model> {
        self.provider_named(Path::new(""), "catalogue").models
    }
}

/// The rate the shipped catalogue records for a model id.
///
/// This is how a row that lost its price gets it back — see
/// [`Provider::adopt_published_rates`]. It answers only for models whose rates
/// this repository actually records, and that limit is the point: the
/// subscription hop has no per-token price at all, and the metered models the
/// presets list without one stay that way. A lookup that can only return what
/// the presets say cannot invent a figure for either.
#[must_use]
pub fn published_price(model_id: &str) -> Option<Price> {
    Preset::ALL
        .iter()
        .flat_map(|preset| preset.models())
        .find(|model| model.id == model_id && !model.deprecated)
        .and_then(|model| model.price)
}

/// Where a provider's key file lives: beside `providers.json`, `0600`.
///
/// Absolute, derived from the same directory the registry itself lives in, so
/// a provider added through the UI resolves to the same file the service
/// reads. A relative path would resolve against whatever the working
/// directory happened to be — which for a systemd unit is `/`.
#[must_use]
pub fn provider_key_path(config_dir: &Path, id: &str) -> PathBuf {
    config_dir.join(format!("provider-{id}.key"))
}

/// Write a provider's key, `0600` from creation.
///
/// Created 0600 rather than chmod'ed afterwards, for the same reason the
/// Claude credential is: a key that is world-readable for the window between
/// write and chmod has been world-readable, and the window is the risk.
///
/// # Errors
/// The directory cannot be created, or the write fails.
pub fn write_provider_key(path: &Path, key: &str) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("key.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| format!("open {}: {e}", tmp.display()))?;
    // Trailing newline: a key pasted into a file brings one, and the reader
    // trims, so writing one costs nothing and makes `cat` behave.
    f.write_all(
        format!(
            "{}
",
            key.trim()
        )
        .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Parse the UI's compact model list: `id:class[:cost]`, comma or newline
/// separated.
///
/// A tiny syntax rather than a nested form, because a provider added by hand
/// is a rare act and a model editor with per-row class dropdowns, cost spinners
/// and add/remove buttons is a great deal of UI to maintain for it. One text
/// field an operator can read at a glance is honest about how often it is used.
///
/// # Errors
/// A line that is not `id:class[:cost]`, naming the line.
pub fn parse_models(spec: &str) -> Result<Vec<Model>, String> {
    let mut out = Vec::new();
    for raw in spec.split([',', '\n']) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(':');
        let id = parts.next().unwrap_or_default().trim();
        let class_text = parts.next().unwrap_or_default().trim();
        let cost = parts.next().map(str::trim);
        if id.is_empty() || class_text.is_empty() {
            return Err(format!("{line:?} is not `id:class[:cost]`"));
        }
        let class = match class_text.to_ascii_lowercase().as_str() {
            "fast" => Class::Fast,
            "balanced" => Class::Balanced,
            "heavy" => Class::Heavy,
            other => return Err(format!("{other:?} is not fast, balanced or heavy")),
        };
        let relative_cost = match cost {
            None => 1,
            Some(c) => c.parse::<u32>().map_err(|_| format!("{c:?} is not a whole number"))?,
        };
        out.push(Model::new(id, class, relative_cost));
    }
    if out.is_empty() {
        return Err("no models listed".to_owned());
    }
    Ok(out)
}

/// Everywhere requests can go.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub providers: Vec<Provider>,
    /// Client model name → the name to use instead. See [`Mapping`].
    #[serde(default)]
    pub mappings: Vec<Mapping>,
}

impl Default for Registry {
    /// Anthropic alone, on the host's own subscription — what the proxy did
    /// before any of this existed, expressed as configuration.
    fn default() -> Self {
        Self {
            providers: vec![Provider {
                id: "anthropic".to_owned(),
                wire: Wire::Anthropic,
                base_url: crate::egress::ANTHROPIC_BASE.to_owned(),
                auth: Auth::ClaudeOauth,
                preference: 0,
                enabled: true,
                peak: None,
                models: vec![
                    Model::new("claude-haiku-4-5-20251001", Class::Fast, 100),
                    Model::new("claude-sonnet-5", Class::Balanced, 300),
                    Model::new("claude-opus-5", Class::Heavy, 1500),
                ],
            }],
            mappings: Vec::new(),
        }
    }
}

impl Registry {
    /// Load `providers.json`, or fall back to [`Registry::default`].
    ///
    /// A malformed file is NOT silently replaced with the default: that would
    /// quietly route everything back to the subscription an operator had
    /// deliberately configured away from. It is reported and the default is
    /// used only because refusing to start would be worse for a proxy whose
    /// job is to keep serving.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        // No file at all is the ordinary first run, not a problem.
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(mut r) if !r.providers.is_empty() => {
                // A row can be missing rates it ought to have: the form cannot
                // express one, and a file older than the field deserialises
                // without them. Put back what the catalogue publishes before
                // anything reads them, so that a provider which was saved once
                // does not cost nothing for ever.
                for p in &mut r.providers {
                    p.adopt_published_rates();
                }
                // Loud at LOAD, not only at the first request that would have
                // used it. A provider that can never be used still sits in the
                // file looking configured, and the operator's next question is
                // "why is my traffic going to the subscription" — which is a
                // far worse place to learn it.
                for p in &r.providers {
                    if let Some(why) = p.unusable_reason() {
                        log::error!("provider {} is UNUSABLE: {why}", p.id);
                    }
                    // And one step quieter: usable, but with no rate recorded
                    // for anything it serves, so its hours will draw no money.
                    // Nothing errors and no token is lost — which is why it has
                    // to be said here. The only symptom is a figure that never
                    // appears, and an operator cannot tell a missing price from
                    // a broken chart.
                    if p.enabled && p.has_no_recorded_rate() {
                        log::warn!(
                            "provider {} serves models with no recorded rate, so its hours show no cost",
                            p.id
                        );
                    }
                }
                r
            }
            Ok(_) => {
                log::warn!("{}: no providers listed; using the built-in default", path.display());
                Self::default()
            }
            Err(e) => {
                log::error!(
                    "{}: {e} — USING THE BUILT-IN DEFAULT, which may not be what you configured",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write the current registry out, so a first run leaves an editable file
    /// rather than a mystery.
    ///
    /// # Errors
    /// The file could not be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Which class a requested model name belongs to.
    ///
    /// Looks through every provider's list first, so a name any of them serves
    /// is classified exactly. Falls back to matching the family in the name,
    /// because a client can ask for a dated or aliased id we have never seen
    /// and "unknown" would be a worse answer than "it says opus".
    #[must_use]
    pub fn class_of(&self, model: &str) -> Option<Class> {
        if let Some(m) = self.providers.iter().flat_map(|p| &p.models).find(|m| m.id == model) {
            return Some(m.class);
        }
        let lower = model.to_ascii_lowercase();
        for (needle, class) in [
            ("opus", Class::Heavy),
            ("sonnet", Class::Balanced),
            ("haiku", Class::Fast),
        ] {
            if lower.contains(needle) {
                return Some(class);
            }
        }
        None
    }

    /// Whether the shared plan is being spent by this proxy at all.
    ///
    /// The question the dashboard asks before it draws a plan at all: with no
    /// usable subscription there is nothing to report, and a panel of empty
    /// figures is worse than no panel. Deliberately the same [`Provider::usable`]
    /// routing uses, so a provider switched off by hand and one left keyless both
    /// take the graph away — otherwise the two would have to be kept in step by
    /// hand, and the graph would eventually claim a plan the router was not
    /// spending.
    #[must_use]
    pub fn subscription_usable(&self) -> bool {
        self.subscription_usable_with(|v| std::env::var(v).ok(), crate::egress::credential_present())
    }

    /// [`Registry::subscription_usable`] with the host state supplied, so the
    /// predicate the dashboard keys on can be tested without a credential file
    /// on the machine running the test.
    #[must_use]
    pub fn subscription_usable_with(
        &self,
        get: impl Fn(&str) -> Option<String> + Copy,
        subscription_present: bool,
    ) -> bool {
        self.providers
            .iter()
            .any(|p| p.uses_the_subscription() && p.usable_with(get, subscription_present))
    }

    /// Every usable `(provider, model)` for a class, best first.
    ///
    /// Ordered by the operator's preference then by cost, so the intended
    /// destination wins and price only breaks ties. Providers that are
    /// disabled or whose credential is missing are left out entirely rather
    /// than tried and failed.
    #[must_use]
    pub fn candidates(&self, class: Class, now: u64) -> Vec<(&Provider, &Model)> {
        self.candidates_with(
            class,
            |v| std::env::var(v).ok(),
            crate::egress::credential_present(),
            now,
        )
    }

    /// [`Registry::candidates`] with the environment supplied, for tests.
    #[must_use]
    pub fn candidates_with(
        &self,
        class: Class,
        get: impl Fn(&str) -> Option<String> + Copy,
        subscription_present: bool,
        now: u64,
    ) -> Vec<(&Provider, &Model)> {
        self.candidates_pinned(class, get, subscription_present, now, None)
    }

    /// Candidates restricted to one provider, when an account is pinned.
    ///
    /// A pin is a statement about where someone's work is allowed to go —
    /// which provider bills it, which jurisdiction holds it, which quota it
    /// spends. It is not a hint, so it filters rather than merely preferring:
    /// an empty pin-set is a 503, which is the honest answer and the one an
    /// operator can see, rather than a silent fall back to the subscription
    /// they were trying to keep this person off.
    #[must_use]
    pub fn candidates_pinned(
        &self,
        class: Class,
        get: impl Fn(&str) -> Option<String> + Copy,
        subscription_present: bool,
        now: u64,
        pinned: Option<&str>,
    ) -> Vec<(&Provider, &Model)> {
        let mut out: Vec<(&Provider, &Model)> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.credential_ready_with(get, subscription_present))
            .filter(|p| pinned.is_none_or(|id| p.id == id))
            .filter_map(|p| p.model_for(class, now).map(|m| (p, m)))
            .collect();
        // Preference first — the operator's stated order beats price, or the
        // subscription would never be used at all.
        out.sort_by_key(|(p, m)| (p.preference, p.cost_at(m, now)));
        out
    }

    /// Providers that serve a model by that exact id, best first.
    ///
    /// This is what makes a mapping a decision rather than a hint: a mapping
    /// to `deepseek-flash` names a model only one provider has, so the request
    /// must go there regardless of preference order.
    #[must_use]
    pub fn providers_serving(
        &self,
        model_id: &str,
        get: impl Fn(&str) -> Option<String> + Copy,
        subscription_present: bool,
        now: u64,
        pinned: Option<&str>,
    ) -> Vec<(&Provider, &Model)> {
        let mut out: Vec<(&Provider, &Model)> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.credential_ready_with(get, subscription_present))
            .filter(|p| pinned.is_none_or(|id| p.id == id))
            .filter_map(|p| p.serves(model_id).map(|m| (p, m)))
            .collect();
        out.sort_by_key(|(p, m)| (p.preference, p.cost_at(m, now)));
        out
    }

    /// What one 1M tokens costs the account holding these pins, in micro-USD.
    ///
    /// The chain mirrors routing rather than inventing a rule: a model pin
    /// outranks a provider pin, and a provider pin is enforced rather than
    /// merely preferred, so an id that names nothing live yields `None` instead
    /// of quietly billing at some other hop's rates.
    ///
    /// **An approximation, and it says so.** A usage bucket records tokens, not
    /// which model produced them, so one rate has to stand for a whole account's
    /// hour. That is exact when an account's traffic lands on one model — which
    /// is what pinned providers and pinned models are for, and what this
    /// deployment does — and wrong for an account left mixing models. The
    /// alternative, costing each bucket from the server's own per-model tallies,
    /// is not available: those are per window, not per hour, so the chart and
    /// the summary tile would disagree.
    ///
    /// Without a provider pin this falls back to the hop routing reaches for
    /// first that can be priced at all. Preference alone decides that, because
    /// routing breaks the tie on the cost of a model the caller has not named
    /// yet — so this is the one place the chain is directionally right rather
    /// than exact.
    ///
    /// The returned rate is off-peak; callers cost an hour through
    /// [`Rate::at`] so peak hours are priced as peak.
    #[must_use]
    pub fn billing_price(&self, provider_pin: Option<&str>, model_pin: Option<&str>) -> Option<Rate> {
        let provider = match provider_pin {
            Some(id) => self.providers.iter().find(|p| p.id == id && p.enabled)?,
            None => self
                .providers
                .iter()
                .filter(|p| p.enabled && p.models.iter().any(|m| !m.deprecated && m.price.is_some()))
                .min_by_key(|p| p.preference)?,
        };
        let model = match model_pin {
            Some(id) => provider.models.iter().find(|m| m.id == id && !m.deprecated)?,
            None => provider
                .models
                .iter()
                .filter(|m| !m.deprecated && m.price.is_some())
                .min_by_key(|m| m.relative_cost)?,
        };
        Some(Rate {
            price: model.price?,
            peak: provider.peak.clone(),
            model: model.id.clone(),
        })
    }

    /// Rewrite a requested model name through the mapping table.
    ///
    /// Follows chains, so `a → b → c` resolves to `c`, and stops on a cycle
    /// rather than looping: a table an operator can edit freely is a table
    /// that will eventually contain `opus → sonnet → opus`, and a router that
    /// hangs on it takes the whole proxy down.
    ///
    /// Returns the name to use and, when it differs, the name that was asked
    /// for — so the response header can say what happened.
    #[must_use]
    pub fn resolve(&self, requested: &str) -> (String, Option<String>) {
        let mut current = requested.to_owned();
        let mut hops = 0;
        while hops < MAX_MAPPING_HOPS {
            let Some(next) = self.mapping_from(&current) else {
                break;
            };
            if next == current {
                break;
            }
            next.clone_into(&mut current);
            hops += 1;
        }
        let changed = current != requested;
        (current, changed.then(|| requested.to_owned()))
    }

    /// The target of a mapping, matched exactly then case-insensitively.
    fn mapping_from(&self, name: &str) -> Option<&str> {
        self.mappings
            .iter()
            .find(|m| m.from == name)
            .or_else(|| self.mappings.iter().find(|m| m.from.eq_ignore_ascii_case(name)))
            .map(|m| m.to.as_str())
    }

    /// Add or replace a provider by id, keeping its position in the list.
    ///
    /// Position is kept so the UI does not reshuffle under an operator every
    /// time they save, and so a preference tie is broken by the order they
    /// arranged rather than by edit recency.
    pub fn upsert(&mut self, provider: Provider) {
        match self.providers.iter_mut().find(|p| p.id == provider.id) {
            Some(slot) => *slot = provider,
            None => self.providers.push(provider),
        }
    }

    /// Forget a provider. Returns whether it was there.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.providers.len();
        self.providers.retain(|p| p.id != id);
        self.providers.len() != before
    }

    /// Set or replace a mapping.
    pub fn set_mapping(&mut self, from: &str, to: &str, note: Option<String>) {
        let existing = self.mappings.iter_mut().find(|m| m.from == from);
        match existing {
            Some(m) => {
                to.clone_into(&mut m.to);
                m.note = note;
            }
            None => self.mappings.push(Mapping {
                from: from.to_owned(),
                to: to.to_owned(),
                note,
            }),
        }
    }

    /// Remove a mapping. Returns whether it was there.
    pub fn remove_mapping(&mut self, from: &str) -> bool {
        let before = self.mappings.len();
        self.mappings.retain(|m| m.from != from);
        self.mappings.len() != before
    }

    /// A count per provider id, for diagnostics.
    #[must_use]
    pub fn summary(&self) -> BTreeMap<String, usize> {
        self.providers.iter().map(|p| (p.id.clone(), p.models.len())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_provider_registry() -> Registry {
        let mut r = Registry::default();
        // The same models from a different quota pool — the case the whole
        // reroute story exists for.
        r.providers.push(Provider {
            peak: None,
            id: "bedrock".to_owned(),
            wire: Wire::Anthropic,
            base_url: "https://bedrock.example".to_owned(),
            auth: Auth::ApiKeyEnv {
                var: "TA_TEST_BEDROCK_KEY".to_owned(),
            },
            preference: 1,
            enabled: true,
            models: vec![
                Model {
                    id: "anthropic.claude-opus-5-v1:0".to_owned(),
                    class: Class::Heavy,
                    relative_cost: 30,
                    price: None,
                    deprecated: false,
                    note: None,
                },
                Model {
                    id: "anthropic.claude-haiku-4-5-v1:0".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
                    price: None,
                    deprecated: false,
                    note: None,
                },
            ],
        });
        r
    }

    #[test]
    fn a_requested_model_maps_to_the_kind_of_work_it_is() {
        let r = two_provider_registry();
        assert_eq!(r.class_of("claude-opus-5"), Some(Class::Heavy));
        assert_eq!(r.class_of("claude-sonnet-5"), Some(Class::Balanced));
        assert_eq!(r.class_of("claude-haiku-4-5-20251001"), Some(Class::Fast));
        // A provider-specific id is known exactly.
        assert_eq!(r.class_of("anthropic.claude-opus-5-v1:0"), Some(Class::Heavy));
        // A dated or aliased name we have never seen still classifies by
        // family — better than refusing to route it.
        assert_eq!(r.class_of("claude-opus-5-20260101"), Some(Class::Heavy));
        assert_eq!(r.class_of("some-other-vendors-model"), None);
    }

    /// A stand-in environment, so no test mutates the process's own.
    fn with_key(v: &str) -> Option<String> {
        (v == "TA_TEST_BEDROCK_KEY").then(|| "a-key".to_owned())
    }
    fn without_key(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn candidates_are_ordered_by_preference_then_cost() {
        let r = two_provider_registry();

        let heavy = r.candidates_with(Class::Heavy, with_key, true, 0);
        assert_eq!(heavy.len(), 2, "both providers serve heavy work");
        assert_eq!(heavy[0].0.id, "anthropic", "the subscription is preference 0");
        assert_eq!(heavy[1].0.id, "bedrock");

        // Nobody serves balanced except anthropic.
        let balanced = r.candidates_with(Class::Balanced, with_key, true, 0);
        assert_eq!(balanced.len(), 1);
        assert_eq!(balanced[0].1.id, "claude-sonnet-5");
    }

    /// A provider whose key is missing must not be offered. Routing to it
    /// produces a 401 from somewhere nobody was looking.
    #[test]
    fn a_provider_without_its_credential_is_not_a_candidate() {
        let r = two_provider_registry();
        let heavy = r.candidates_with(Class::Heavy, without_key, true, 0);
        assert_eq!(heavy.len(), 1, "bedrock has no key, so it is not offered");
        assert_eq!(heavy[0].0.id, "anthropic");
    }

    /// The subscription's credential is a file on this host rather than a key,
    /// so it is a second way to have nothing to authenticate with. A bare `true`
    /// here is what routed requests into an egress that then failed to read
    /// `.credentials.json`, once per poll.
    #[test]
    fn the_subscription_is_not_a_candidate_without_its_credential_file() {
        let r = Registry::default();
        assert!(
            r.candidates_with(Class::Balanced, with_key, false, 0).is_empty(),
            "a subscription with no credential file has nothing to authenticate with"
        );
        assert_eq!(
            r.candidates_with(Class::Balanced, with_key, true, 0).len(),
            1,
            "and is a candidate again the moment the file is there"
        );
    }

    /// Both ways of being disabled land in the same place, which is what lets
    /// the dashboard key the plan panel on this one question.
    #[test]
    fn usable_is_false_for_a_switch_off_and_for_a_missing_credential() {
        let mut on = Registry::default().providers.remove(0);
        assert!(
            on.usable_with(without_key, true),
            "enabled, and its credential file is there"
        );
        assert!(
            !on.usable_with(without_key, false),
            "the same provider with no credential file has nothing to spend the plan with"
        );
        assert_eq!(
            on.credential_ready_with(with_key, false),
            on.credential_ready_with(without_key, false),
            "the subscription reads its file, not the env: a key in it must not stand in"
        );
        assert!(
            on.credential_ready_with(with_key, true),
            "and a key being present must not be what makes it ready either"
        );
        on.enabled = false;
        assert!(!on.usable_with(without_key, true), "switched off, file and all");
    }

    /// The dashboard asks this rather than the credential directly, so that a
    /// provider switched off by hand hides the graph exactly as a missing
    /// credential file does.
    #[test]
    fn the_plan_is_reported_on_only_while_the_subscription_is_usable() {
        let mut r = Registry::default();
        assert!(r.subscription_usable_with(without_key, true), "on and ready");
        assert!(
            !r.subscription_usable_with(without_key, false),
            "on, but with no credential file to authenticate with"
        );
        r.providers[0].enabled = false;
        assert!(
            !r.subscription_usable_with(without_key, true),
            "switched off by hand takes the graph with it"
        );
    }

    /// The refusal a caller is shown must name the thing to fix. "no provider
    /// available" is true but sends the operator hunting for a routing fault
    /// when the answer is a file that is not there.
    #[test]
    fn a_keyless_subscription_refusal_names_the_file_that_would_fix_it() {
        let p = Registry::default().providers.remove(0);
        let why = p
            .refusal_with(without_key, false)
            .expect("a subscription with no credential file cannot serve");
        assert!(why.contains("anthropic"), "the provider is not named: {why}");
        assert!(
            why.contains(".credentials.json"),
            "the file that would fix it is not named: {why}"
        );
        assert!(
            p.refusal_with(without_key, true).is_none(),
            "and there is nothing to refuse once it is there"
        );
    }

    #[test]
    fn a_disabled_provider_is_not_a_candidate() {
        let mut r = two_provider_registry();
        for p in &mut r.providers {
            if p.id == "bedrock" {
                p.enabled = false;
            }
        }
        assert_eq!(r.candidates_with(Class::Heavy, with_key, true, 0).len(), 1);
    }

    #[test]
    fn the_default_registry_is_the_subscription_alone() {
        let r = Registry::default();
        assert_eq!(r.providers.len(), 1);
        assert_eq!(r.providers[0].auth, Auth::ClaudeOauth);
        // Every class is served, or a request could arrive with nowhere to go.
        for class in Class::LADDER {
            assert_eq!(
                r.candidates_with(class, without_key, true, 0).len(),
                1,
                "no candidate for {class:?}"
            );
        }
    }

    #[test]
    fn the_class_ladder_only_goes_down() {
        assert_eq!(Class::Heavy.cheaper(), Some(Class::Balanced));
        assert_eq!(Class::Balanced.cheaper(), Some(Class::Fast));
        assert_eq!(Class::Fast.cheaper(), None);
    }

    /// The file a provider reads its key from.
    fn key_path(p: &Provider) -> String {
        match &p.auth {
            Auth::ApiKeyFile { path } => path.clone(),
            _ => panic!("every preset authenticates from a key file"),
        }
    }

    /// Two API keys for the same provider coexist, each with its own file.
    ///
    /// The multi-key case: one key's quota is not enough, so the operator adds
    /// a second and pins one account to each. The ids must differ, because the
    /// id is what gives each key a file of its own — one id is one path, and
    /// the key written last would be the only one left.
    #[test]
    fn a_second_preset_key_gets_its_own_id_and_key_file() {
        let dir = Path::new("/var/lib/tab-atelier-proxy");
        let mut registry = Registry::default();

        let first = Preset::Deepseek.provider(dir);
        let first_key = key_path(&first);
        registry.upsert(first);

        // Told the plain name is taken, the second request takes the next one.
        let second = Preset::Deepseek.provider_for(dir, registry.providers.iter().map(|p| p.id.as_str()));
        let second_key = key_path(&second);
        registry.upsert(second);

        // The default registry already holds `anthropic`, so filter to the
        // entries this test actually made.
        let both: Vec<&str> = registry
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .filter(|id| id.starts_with("deepseek"))
            .collect();
        assert_eq!(both, ["deepseek", "deepseek-2"], "the second key joined the first");
        assert_ne!(first_key, second_key, "one path for two keys means the last write wins");
    }

    /// Numbering steps over the names already in use, not just the first.
    #[test]
    fn a_further_key_skips_the_names_in_use() {
        let dir = Path::new("/var/lib/tab-atelier-proxy");
        let third = Preset::Deepseek.provider_for(dir, ["deepseek", "deepseek-2"]);
        assert_eq!(third.id, "deepseek-3");
        assert!(
            key_path(&third).ends_with("provider-deepseek-3.key"),
            "the file is named after the id, so distinct ids keep keys apart"
        );
    }

    /// An unclaimed name is used as-is, so the first entry stays plain.
    #[test]
    fn an_unclaimed_preset_keeps_its_plain_id() {
        let p = Preset::Deepseek.provider_for(Path::new("/tmp"), std::iter::empty::<&str>());
        assert_eq!(p.id, "deepseek");
    }

    /// `DeepSeek`'s published schedule, against real instants.
    ///
    /// Peak is 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday, at double.
    /// The boundaries are the interesting part: an off-by-one on the
    /// half-open range would either double the price an hour early or miss an
    /// hour of it, and neither shows up until a bill does.
    #[test]
    fn peak_windows_follow_the_providers_published_schedule() {
        let ds = Preset::Deepseek.provider(Path::new("/tmp"));
        let flash = ds.models.iter().find(|m| m.id == "deepseek-flash").expect("flash");
        let off_peak = flash.relative_cost;

        // Thursday 02:00 UTC — inside the first window.
        assert!(ds.peak_now(1_789_005_600), "Thu 02:00 UTC is peak");
        // Thursday 05:00 UTC — between the two windows.
        assert!(!ds.peak_now(1_789_016_400), "Thu 05:00 UTC is off-peak");
        // Thursday 00:30 UTC — before the first window opens.
        assert!(!ds.peak_now(1_789_000_200), "the window is half-open at the start");
        // Thursday 09:30 UTC — inside the second window.
        assert!(ds.peak_now(1_789_032_600), "Thu 09:30 UTC is peak");
        // Thursday 12:00 UTC — after it closes.
        assert!(!ds.peak_now(1_789_041_600), "the window is half-open at the end");
        // Saturday 02:00 UTC — same clock time, different day.
        assert!(!ds.peak_now(1_789_178_400), "peak is weekday-only");

        // And the price actually moves with it, which is the point of having
        // the schedule at all rather than a number in a comment.
        assert_eq!(ds.cost_at(flash, 1_789_005_600), off_peak * 2);
        assert_eq!(ds.cost_at(flash, 1_789_016_400), off_peak);
    }

    /// "Peak now" without an end is a warning nobody can plan around. The end
    /// must land on the window's boundary hour, not on the request instant
    /// rounded forward — that would be up to 59 minutes late.
    #[test]
    fn an_active_window_reports_when_it_ends() {
        let ds = Preset::Deepseek.provider(Path::new("/tmp"));

        // Thursday 09:30 UTC, halfway through the 06:00–10:00 window.
        let until = ds.peak_until(1_789_032_600).expect("an active window has an end");
        assert_eq!(until, 1_789_034_400, "ends at 10:00 UTC, not at 09:59 or 10:30");
        // Half-open: still in force the second before, off at the boundary.
        assert!(ds.peak_now(until - 1));
        assert!(!ds.peak_now(until));

        // Off-peak has no end to show — the UI would otherwise print a time
        // for a surcharge that is not being charged.
        assert_eq!(ds.peak_until(1_789_016_400), None, "Thu 05:00 UTC is off-peak");
        assert_eq!(ds.peak_until(1_789_178_400), None, "Saturday is off-peak too");
    }

    /// A withdrawn model must not be offered, however cheap it looks.
    #[test]
    fn a_deprecated_model_is_never_a_candidate() {
        let ds = Preset::Deepseek.provider(Path::new("/tmp"));
        // It is listed, so an operator can see it existed and why it is gone…
        let pro = ds.models.iter().find(|m| m.id == "deepseek-v4-pro").expect("listed");
        assert!(pro.deprecated);
        assert!(pro.note.as_deref().is_some_and(|n| n.contains("2026-09-14")));
        // …and it is skipped. It still ACCEPTS traffic — requests to it are
        // served by a different model at a different price after the 14th —
        // so a router that kept offering it would report a cost and a
        // capability that are both about to stop being true.
        assert!(ds.serves("deepseek-v4-pro").is_none());
        assert!(
            ds.model_for(Class::Heavy, 0).is_none(),
            "its only heavy model is withdrawn"
        );
        assert!(ds.serves("deepseek-flash").is_some());
    }

    /// The preset must be usable the moment it is added, or the first request
    /// after an operator clicks "add" fails for a reason they cannot see.
    #[test]
    fn the_deepseek_preset_is_complete_and_points_at_the_anthropic_endpoint() {
        let ds = Preset::Deepseek.provider(Path::new("/var/lib/tab-atelier-proxy"));
        assert_eq!(ds.id, "deepseek");
        assert_eq!(ds.wire, Wire::Anthropic);
        // NOT the bare host: DeepSeek serves an OpenAI-shaped API there, and
        // pointing at it would mangle every tool call rather than fail.
        assert_eq!(ds.base_url, "https://api.deepseek.com/anthropic");
        assert!(ds.enabled);
        // The key lives in its own file — never in providers.json.
        match &ds.auth {
            Auth::ApiKeyFile { path } => {
                assert!(
                    path.starts_with('/'),
                    "an absolute path: a relative one resolves to / under systemd"
                );
                assert!(path.ends_with("provider-deepseek.key"), "{path}");
            }
            other => panic!("expected a file credential, got {other:?}"),
        }
        // After the subscription, which is already paid for.
        assert!(ds.preference > Registry::default().providers[0].preference);
    }

    #[test]
    fn the_published_prices_agree_with_the_routing_primitive() {
        // `relative_cost` is the miss-input rate in *cents* per 1M and `Price`
        // holds *micro*-USD per 1M, so the two are the same number four decimal
        // places apart. Pinning that here is what makes a slip in either unit
        // fail loudly instead of quietly costing 1000x too little: the display
        // and the router would otherwise drift with nothing to compare them.
        let ds = Preset::Deepseek.provider(Path::new("/var/lib/tab-atelier-proxy"));
        let mut priced = 0;
        for model in &ds.models {
            let Some(price) = model.price else {
                continue;
            };
            assert_eq!(
                price.input / 10_000,
                model.relative_cost,
                "{}: ${}/1M miss-input is {} cents, not {}",
                model.id,
                f64::from(price.input) / 1_000_000.0,
                price.input / 10_000,
                model.relative_cost,
            );
            // And the classes must stay ordered the way the provider prices
            // them, or a cheap-looking total is hiding an inverted lookup.
            assert!(
                price.cache_hit < price.input && price.input < price.output,
                "{}: hit {} < miss {} < out {} is the shape of every published table",
                model.id,
                price.cache_hit,
                price.input,
                price.output,
            );
            priced += 1;
        }
        assert!(priced > 0, "the preset ships at least one priced model");

        // The one model deliberately left unpriced, because it is withdrawn and
        // its traffic is served by Flash at Flash prices.
        let pro = ds.models.iter().find(|m| m.id.contains("v4-pro"));
        assert!(
            pro.is_some_and(|m| m.price.is_none()),
            "a withdrawn model must stay unpriced, not be costed at its own rates"
        );
    }

    /// The one test with an outside source of truth: a bill this proxy produced.
    ///
    /// The first real period reported 176M uncached tokens, 2.94B cached, and a
    /// cache-miss line of $29.20 that was 63% of the total. Feeding those counts
    /// through the table has to land on the same shape, or the table is not the
    /// one the vendor charged by. It is also what fixes the rates' *units*:
    /// `micro` per 1M is only right because this arithmetic comes out in
    /// dollars.
    #[test]
    fn the_rates_reproduce_the_reported_bill() {
        let ds = Preset::Deepseek.provider(Path::new("/var/lib/tab-atelier-proxy"));
        let price = ds.models[0].price.expect("flash is priced");

        let miss = price.cost_micro(0, 176_000_000, 0);
        let hit = price.cost_micro(2_940_000_000, 0, 0);
        assert_eq!(miss, 26_400_000, "176M uncached at $0.15/1M is $26.40");
        assert_eq!(hit, 8_820_000, "2.94B cached at $0.003/1M is $8.82");

        // The reported miss line was $29.20, not $26.40, and the gap is the
        // reason the peak multiplier is part of the model rather than a note:
        // $29.20/176M is $0.166/1M, which no flat rate in the table produces,
        // but which 10.6% of the volume at double does. Pricing this at
        // off-peak rates alone would under-report it by 10%.
        let peak_share = 0.106;
        let with_peak = miss + (miss * 106 / 1000);
        assert_eq!(with_peak, 29_198_400);
        assert!(
            (with_peak - 29_200_000).abs() < 100_000,
            "peak-inclusive miss should land on the reported $29.20"
        );
        assert!((0.0..0.25).contains(&peak_share), "and on a plausible peak share");

        // 63% of the bill, with output at $0.60/1M as the remainder.
        let out = price.cost_micro(0, 0, 12_400_000);
        let total = miss + hit + out;
        let share = miss * 100 / total;
        assert!(
            (60..=66).contains(&share),
            "the miss line was 63% of the total, this table gives {share}%"
        );
    }

    #[test]
    fn a_hit_costs_far_less_than_a_miss_which_is_why_the_split_exists() {
        // The measured gap is 50x on Flash. If a change ever made the two equal,
        // the input panel's money figure would stop being dominated by misses
        // and the whole reason for splitting `cost_in` from `cost_out` would be
        // gone — so the assumption is asserted rather than described.
        let ds = Preset::Deepseek.provider(Path::new("/var/lib/tab-atelier-proxy"));
        let flash = ds.models.iter().find(|m| m.id == "deepseek-flash").expect("flash");
        let price = flash.price.expect("priced");
        assert_eq!(price.cache_hit * 50, price.input);
    }

    #[test]
    fn the_deepseek_preset_records_published_off_peak_rates() {
        // Off-peak, from the published table quoted at the top of the preset:
        // $0.003 hit / $0.15 miss / $0.60 out per 1M. Stored in micro-USD, so
        // the same figures a factor of a million larger.
        let ds = Preset::Deepseek.provider(Path::new("/var/lib/tab-atelier-proxy"));
        let flash = ds.models.iter().find(|m| m.id == "deepseek-flash").expect("flash");
        let price = flash.price.expect("flash is priced");
        assert_eq!(price.cache_hit, 3_000, "$0.003/1M");
        assert_eq!(price.input, 150_000, "$0.15/1M");
        assert_eq!(price.output, 600_000, "$0.60/1M");

        // And the peak schedule that doubles them, which the cost of an hour
        // depends on as much as the rates do.
        let peak = ds.peak.as_ref().expect("deepseek has a peak schedule");
        assert_eq!(peak.multiplier_percent, 200);
        assert_eq!(peak.windows.len(), 2, "01:00-04:00 and 06:00-10:00");
        assert!(peak.windows.iter().all(|w| w.weekdays == vec![1, 2, 3, 4, 5]));

        // Peak doubles all three classes, the cached one included: the
        // published table gives $0.006 / $0.30 / $1.20 against $0.003 /
        // $0.15 / $0.60. Exempting cache hits is the natural guess — a
        // discount you would expect to survive peak — and it is wrong, which
        // is why `scaled` multiplies `cache_hit` rather than leaving it.
        let at_peak = price.scaled(peak.multiplier_percent);
        assert_eq!(at_peak.cache_hit, 6_000, "$0.006/1M at peak");
        assert_eq!(at_peak.input, 300_000, "$0.30/1M at peak");
        assert_eq!(at_peak.output, 1_200_000, "$1.20/1M at peak");
    }

    #[test]
    fn a_file_credential_is_read_per_request_and_missing_is_not_ready() {
        let dir = std::env::temp_dir().join(format!("ta-prov-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("provider-x.key");
        let auth = Auth::ApiKeyFile {
            path: path.display().to_string(),
        };

        assert!(auth.secret_with(|_| None).is_err(), "no file yet");
        std::fs::write(&path, "sk-live\n").expect("write");
        // Trimmed: a key pasted into a file brings a newline, and a credential
        // with a trailing \n in it fails as "invalid key" from the far end.
        assert_eq!(auth.secret_with(|_| None).expect("read"), "sk-live");

        // Rotating is a file write, not a restart — which is the whole reason
        // the secret is not captured at startup.
        std::fs::write(&path, "sk-rotated").expect("write");
        assert_eq!(auth.secret_with(|_| None).expect("read"), "sk-rotated");

        std::fs::write(&path, "   ").expect("write");
        assert!(auth.secret_with(|_| None).is_err(), "whitespace is not a key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "When someone asks for X, use Y" — across providers and within one.
    #[test]
    fn a_mapping_rewrites_the_name_and_terminates_whatever_the_table_says() {
        let mut r = Registry::default();
        r.set_mapping("claude-opus-5", "claude-sonnet-5", Some("cost control".to_owned()));
        let (name, from) = r.resolve("claude-opus-5");
        assert_eq!(name, "claude-sonnet-5");
        assert_eq!(from.as_deref(), Some("claude-opus-5"));

        // A mapping is stronger than the family heuristic: class_of would
        // still say Heavy from the word "opus", but the resolved name is what
        // routing now classifies.
        assert_eq!(r.class_of(&name), Some(Class::Balanced));

        // Nothing mapped is unchanged, and reports no change.
        assert_eq!(
            r.resolve("claude-haiku-4-5-20251001"),
            ("claude-haiku-4-5-20251001".to_owned(), None)
        );

        // A chain resolves to its end…
        r.set_mapping("a", "b", None);
        r.set_mapping("b", "c", None);
        assert_eq!(r.resolve("a").0, "c");

        // …and a cycle stops instead of hanging. An operator-editable table
        // WILL eventually contain opus → sonnet → opus, and a router that
        // loops on it takes the whole proxy down with it.
        r.set_mapping("x", "y", None);
        r.set_mapping("y", "x", None);
        let (name, _) = r.resolve("x");
        assert!(name == "x" || name == "y", "must terminate, got {name}");

        // Replacing and removing.
        r.set_mapping("a", "z", None);
        assert_eq!(r.resolve("a").0, "z");
        assert!(r.remove_mapping("a"));
        assert!(!r.remove_mapping("a"), "second removal is a no-op");
    }

    /// A provider whose credential is not a file must refuse a key write
    /// rather than accept one into a file nothing reads.
    /// The one combination that leaks a credential, refused wherever a
    /// provider is considered.
    #[test]
    fn claude_oauth_is_only_ever_pointed_at_anthropic() {
        // `claude_oauth` means "send the proxy host's own Claude access token".
        // Pointed anywhere else, every request hands the subscription
        // credential to a third party — and succeeds, so nothing looks wrong.
        let mut p = Provider {
            id: "not-anthropic".to_owned(),
            wire: Wire::Anthropic,
            base_url: "https://evil.example/v1".to_owned(),
            auth: Auth::ClaudeOauth,
            models: vec![Model::new("m", Class::Balanced, 1)],
            preference: 0,
            enabled: true,
            peak: None,
        };
        let why = p
            .unusable_reason()
            .expect("a non-Anthropic host with an OAuth credential");
        assert!(why.contains("evil.example"), "the message names the destination: {why}");
        // Not merely reported — excluded, or it would still be chosen.
        assert!(!p.credential_ready_with(|_| Some("k".to_owned()), true));
        let r = Registry {
            providers: vec![p.clone()],
            mappings: vec![],
        };
        assert!(
            r.candidates_with(Class::Balanced, |_| Some("k".to_owned()), true, 0)
                .is_empty(),
            "an unusable provider must not be a candidate, whatever else is configured"
        );

        // The same provider pointing at Anthropic is fine.
        p.base_url = "https://api.anthropic.com".to_owned();
        assert!(p.unusable_reason().is_none());
        assert!(
            p.credential_ready_with(|_| None, true),
            "and needs no key, but does need its credential file"
        );
        assert!(
            !p.credential_ready_with(|_| None, false),
            "without which it cannot spend the plan at all"
        );

        // A trailing slash or a different case is the same host, not a
        // different one — a check that refused these would be a bug of its own.
        for ok in [
            "https://api.anthropic.com/",
            "https://API.ANTHROPIC.COM",
            "https://api.anthropic.com",
        ] {
            p.base_url = ok.to_owned();
            assert!(p.unusable_reason().is_none(), "{ok} is Anthropic");
        }

        // A file-backed provider may point anywhere: its credential is its own.
        p.auth = Auth::ApiKeyFile {
            path: "/tmp/k".to_owned(),
        };
        p.base_url = "https://api.deepseek.com/anthropic".to_owned();
        assert!(p.unusable_reason().is_none());
    }

    #[test]
    fn host_of_takes_the_host_and_nothing_else() {
        assert_eq!(host_of("https://api.anthropic.com/v1/messages"), "api.anthropic.com");
        assert_eq!(host_of("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(host_of("https://API.Anthropic.com:443/x"), "api.anthropic.com:443");
        // No scheme at all still yields the host rather than the whole URL,
        // which is the shape a comparison silently fails on.
        assert_eq!(host_of("api.anthropic.com/v1"), "api.anthropic.com");
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn only_a_file_backed_provider_can_be_given_a_key() {
        // The predicate the ROUTE calls, not a reimplementation of it — a test
        // that restates the rule passes happily while the route does something
        // else, which is the whole failure mode worth avoiding here.
        let file = Auth::ApiKeyFile {
            path: "/tmp/x.key".to_owned(),
        };
        assert!(file.accepts_a_key());
        assert!(file.no_key_reason("x").is_none());

        // The subscription's credential is the host's Claude login. A key file
        // would never be read, and would sit on disk as a live secret nothing
        // consults.
        assert!(!Auth::ClaudeOauth.accepts_a_key());
        let why = Auth::ClaudeOauth.no_key_reason("anthropic").expect("a reason");
        assert!(
            why.contains("import-credentials"),
            "the refusal says what to do instead: {why}"
        );

        // An env-backed provider reads the environment; a file would be ignored.
        let env = Auth::ApiKeyEnv {
            var: "BEDROCK_KEY".to_owned(),
        };
        assert!(!env.accepts_a_key());
        let why = env.no_key_reason("bedrock").expect("a reason");
        assert!(why.contains("BEDROCK_KEY"), "it names the variable: {why}");
    }

    /// The form is a partial view, and a partial view must save partially.
    #[test]
    fn updating_a_provider_keeps_what_the_form_cannot_express() {
        let r = Registry::default();
        let original = r.providers[0].clone();
        assert_eq!(original.auth, Auth::ClaudeOauth);

        // What the form would rebuild: a base URL, models and a file
        // credential, because that is all it has fields for.
        let from_form = Provider {
            auth: Auth::ApiKeyFile {
                path: "/tmp/whatever.key".to_owned(),
            },
            peak: None,
            preference: 99,
            enabled: false,
            ..original.clone()
        };

        // The merge `save_provider` performs.
        let mut merged = from_form;
        let old = r.providers[0].clone();
        merged.preference = old.preference;
        merged.enabled = old.enabled;
        merged.auth = old.auth.clone();
        merged.peak.clone_from(&old.peak);

        // Without this, saving the subscription's own row turned it into a
        // keyless file-backed provider: not ready, not a candidate, and the
        // whole proxy with nothing to serve from.
        assert_eq!(merged.auth, Auth::ClaudeOauth, "auth must survive a form save");
        assert!(merged.credential_ready_with(|_| None, true), "and it is still ready");
        assert_eq!(merged.preference, 0);
        assert!(merged.enabled);
        // What the form DOES express still takes effect.
        assert_eq!(merged.base_url, original.base_url);
    }

    /// The `QoS` scheduler measures ONE quota, and this is the line it is drawn
    /// along.
    ///
    /// Regression: `on_429` set a GLOBAL backoff and `admit` gated every
    /// request, so a saturated subscription returned "the shared quota is
    /// saturated; retry in 172s" for traffic bound somewhere else entirely —
    /// and disabling the subscription was the only way to get that traffic
    /// moving again. The reverse also held: a far end refusing traffic put the
    /// subscription's own requests on hold.
    #[test]
    fn only_the_subscription_provider_spends_the_subscription() {
        assert!(
            Registry::default().providers[0].uses_the_subscription(),
            "the host's own Claude login IS the plan"
        );

        // A provider with its own credential bills separately, whatever its
        // base URL or what it is called.
        for auth in [
            Auth::ApiKeyFile {
                path: "/tmp/k".to_owned(),
            },
            Auth::ApiKeyEnv { var: "K".to_owned() },
        ] {
            let p = Provider {
                id: "somewhere-else".to_owned(),
                wire: Wire::Anthropic,
                base_url: "https://api.deepseek.com/anthropic".to_owned(),
                auth,
                models: vec![Model::new("m", Class::Balanced, 1)],
                preference: 10,
                enabled: true,
                peak: None,
            };
            assert!(
                !p.uses_the_subscription(),
                "a keyed provider must not be gated by the plan"
            );
        }

        // "Spends the plan" and "is the subscription" cannot drift apart: the
        // first is literally the credential the second is tied to.
        for p in &Registry::default().providers {
            assert_eq!(
                p.uses_the_subscription(),
                p.auth == Auth::ClaudeOauth,
                "{} disagrees with its own credential",
                p.id
            );
        }
    }

    /// Compaction is a PER-PERSON setting, not a per-provider one, with a
    /// default that must not move.
    ///
    /// It reads like a property of the hop, because the harm it can do is a
    /// property of the hop — see [`Provider::compact_refusal`]. But the
    /// operator reasoning about it is looking at a person ("Mallory is costing
    /// us a fortune in context she has stopped needing"), the hop is chosen
    /// per request by routing, and a level filed under a provider silently
    /// changes meaning the moment that provider is no longer where her traffic
    /// goes. So the level lives on the account and the hop's objection is
    /// raised against whatever route was actually taken.
    #[test]
    fn compact_round_trips_and_defaults_to_none() {
        let dir = std::env::temp_dir().join(format!("ta-compact-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("users.json");
        let mut store = crate::users::Store::load(&path).expect("empty store");
        store.add("Who", "Ever", "who@example.com").expect("add");
        let id = store.accounts()[0].id.clone();

        assert!(
            store.find(&id).expect("added").compact.is_none(),
            "compaction must default off"
        );

        for level in crate::compact::Compact::ALL {
            store.set_compact(&id, level).expect("set");
            // Reloaded from disk rather than round-tripped in memory: what has
            // to hold is that a RESTARTED service reads back the level an
            // operator chose, and only the file knows that.
            let back = crate::users::Store::load(&path).expect("reload");
            assert_eq!(back.find(&id).expect("round-tripped").compact, level, "{level:?}");
            // The spelling in the file is the one the UI and the docs use.
            let json = std::fs::read_to_string(&path).expect("read");
            assert!(json.contains(level.as_str()), "{json}");
        }

        // A file written before the field existed has no `compact` key and must
        // read as `none` rather than failing to load: a file that does not
        // parse costs every key in it, which is every login in the file.
        let legacy = dir.join("legacy.json");
        std::fs::write(
            &legacy,
            r#"{"accounts":[{"id":"a","first_name":"A","last_name":"B",
                "email":"a@b.c","created_at":0,"keys":[]}]}"#,
        )
        .expect("write");
        let old = crate::users::Store::load(&legacy).expect("a pre-compaction file still parses");
        assert!(old.accounts()[0].compact.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The combination the spec calls out as impossible to be correct.
    #[test]
    fn compaction_is_refused_on_the_anthropic_login() {
        let mut r = Registry::default();
        let anthropic = &r.providers[0];
        assert_eq!(anthropic.auth, Auth::ClaudeOauth);

        // `none` is always fine.
        assert!(anthropic.compact_refusal(crate::compact::Compact::None).is_none());

        // Anything else is not, and the message says why in the terms that
        // matter: a rewritten body invalidates the cache breakpoints that make
        // the subscription affordable.
        for level in [
            crate::compact::Compact::Tools,
            crate::compact::Compact::ToolsThinking,
            crate::compact::Compact::All,
        ] {
            let why = anthropic
                .compact_refusal(level)
                .unwrap_or_else(|| panic!("{level:?} must be refused on claude_oauth"));
            assert!(why.contains("cache"), "{why}");
            assert!(why.contains("none"), "the refusal says what to use instead: {why}");
        }

        // A provider with its own credential is free to compact: the cache it
        // would invalidate is not Anthropic's to have kept.
        let mut deepseek = crate::provider::Preset::Deepseek.provider(Path::new("/tmp"));
        deepseek.auth = Auth::ApiKeyFile {
            path: "/tmp/k".to_owned(),
        };
        for level in crate::compact::Compact::ALL {
            assert!(
                deepseek.compact_refusal(level).is_none(),
                "{level:?} must be allowed on a keyed provider"
            );
        }
        r.providers.push(deepseek);
    }

    #[test]
    fn a_registry_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!("ta-proxy-prov-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("providers.json");

        let r = two_provider_registry();
        r.save(&path).expect("save");
        let back = Registry::load(&path);
        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.summary()["bedrock"], 2);

        // A broken file falls back to the default rather than refusing to
        // start — but it is loud about it, because the default is not what the
        // operator configured.
        std::fs::write(&path, "{ not json").expect("write");
        assert_eq!(Registry::load(&path).providers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The repair can only hand back what this repository actually records.
    ///
    /// That limit is the whole safety argument for it: a model left unpriced on
    /// purpose must stay unpriced, or the dashboard starts stating costs nobody
    /// published. Each of the four answers here is a claim about the catalogue,
    /// so a preset that gains or drops a rate will fail this test rather than
    /// silently widen what the repair is willing to invent.
    #[test]
    fn only_the_rate_the_catalogue_publishes_is_handed_back() {
        assert!(
            published_price("deepseek-flash").is_some(),
            "the one priced model in the tree must answer, or the repair does nothing"
        );
        for unpriced in [
            // No triple to hand back, for two different reasons. The preset
            // records this one's published rates in a comment and deliberately
            // leaves `price` unset, because `billing_price` skips deprecated
            // models and a price there could never be selected for billing.
            "deepseek-v4-pro",
            // And this one is listed with no rate anywhere at all.
            "gpt-5.6-sol",
            // The subscription hop has no per-token cost at all.
            "claude-opus-5",
            // And nothing that was never heard of.
            "no-such-model",
        ] {
            assert!(
                published_price(unpriced).is_none(),
                "{unpriced} has no rate recorded in the catalogue; \
                 a repair must not manufacture one"
            );
        }
    }

    /// A file whose rows lost their rates gets them back as it is read.
    ///
    /// This is the case that makes the money unit work again for a provider
    /// already in use: it was saved through the form, so its `price` is gone
    /// from disk, and no later save can bring it back on its own.
    #[test]
    fn loading_a_row_that_lost_its_rate_takes_the_catalogue_back() {
        let dir = std::env::temp_dir().join(format!("ta-proxy-rate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("providers.json");

        let mut r = Registry::default();
        let mut deepseek = Preset::Deepseek.provider(&dir);
        let published = deepseek
            .models
            .iter()
            .find(|m| m.id == "deepseek-flash")
            .and_then(|m| m.price)
            .expect("the preset ships a rate");
        for model in &mut deepseek.models {
            model.price = None;
        }
        r.providers.push(deepseek);
        r.save(&path).expect("save");

        let back = Registry::load(&path);
        let restored = back
            .providers
            .iter()
            .find(|p| p.id == "deepseek")
            .and_then(|p| p.models.iter().find(|m| m.id == "deepseek-flash"))
            .and_then(|m| m.price);
        assert_eq!(
            restored,
            Some(published),
            "a row saved without a rate must come back with the published one"
        );

        // And the row that was never priced stays that way, so the repair
        // cannot be mistaken for a general "price everything" pass.
        let anthropic = back
            .providers
            .iter()
            .find(|p| p.id == "anthropic")
            .expect("the default row is still there");
        assert!(
            anthropic.has_no_recorded_rate(),
            "the subscription hop records no rate, and none is invented for it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rate set by hand is not the repair's to change.
    ///
    /// Only an absent one is filled, so an operator who priced a model the
    /// catalogue does not know keeps their figure — and gets to keep it through
    /// every later save, since the repair and the save-preserve agree on it.
    #[test]
    fn a_recorded_rate_is_left_alone_by_the_repair() {
        let mine = Price {
            cache_hit: 1,
            input: 3,
            output: 4,
        };
        let mut p = Provider {
            id: "by-hand".to_owned(),
            wire: Wire::Anthropic,
            base_url: "https://api.deepseek.com/anthropic".to_owned(),
            auth: Auth::ApiKeyEnv { var: "K".to_owned() },
            models: vec![Model::new("deepseek-flash", Class::Balanced, 15).priced(1, 3, 4)],
            preference: 10,
            enabled: true,
            peak: None,
        };
        p.adopt_published_rates();
        assert_eq!(
            p.models[0].price,
            Some(mine),
            "the catalogue overwrote a rate that was already recorded"
        );

        // The same row with the rate missing does take the published one, which
        // is what separates this from a no-op.
        p.models[0].price = None;
        p.adopt_published_rates();
        assert_eq!(p.models[0].price, published_price("deepseek-flash"));
        assert!(p.models[0].price.is_some(), "and there was one to take");
    }
}
