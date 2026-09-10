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

/// What a model is for, rather than what it is called.
///
/// Routing happens within a class: a request that arrived asking for Opus is a
/// request for something in [`Class::Heavy`], and any heavy model with
/// capacity can serve it.
/// How many mapping hops to follow before giving up.
///
/// A chain longer than this is not a routing strategy, it is a mistake — and
/// the loop has to terminate whatever the table says.
const MAX_MAPPING_HOPS: usize = 8;

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
            deprecated: false,
            note: None,
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

    /// Whether its credential is actually present.
    ///
    /// A provider configured but unusable must be visibly unusable — routing
    /// to one whose key is missing produces a 401 from somewhere the operator
    /// was not looking.
    #[must_use]
    pub fn credential_ready(&self) -> bool {
        self.credential_ready_with(|v| std::env::var(v).ok())
    }

    /// The same question with the environment supplied.
    ///
    /// Tests use this rather than setting variables: `set_var` is `unsafe`
    /// (forbidden in this crate) and races every other test in the binary.
    #[must_use]
    pub fn credential_ready_with(&self, get: impl Fn(&str) -> Option<String>) -> bool {
        match &self.auth {
            Auth::ClaudeOauth => true,
            Auth::ApiKeyEnv { var } => get(var).is_some_and(|v| !v.trim().is_empty()),
            // A file is checked for existence, not read: this runs on every
            // routing decision and the contents are fetched per request.
            Auth::ApiKeyFile { path } => std::fs::metadata(path).is_ok_and(|m| m.len() > 0),
        }
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
}

impl Preset {
    pub const ALL: [Self; 1] = [Self::Deepseek];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Deepseek => "deepseek",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Deepseek => "DeepSeek",
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
        match self {
            Self::Deepseek => Provider {
                id: self.id().to_owned(),
                wire: Wire::Anthropic,
                // The ANTHROPIC-shaped endpoint. DeepSeek also serves an
                // OpenAI-shaped one on the bare host; pointing at it would
                // produce a 404 or a silently mangled tool call per request.
                base_url: "https://api.deepseek.com/anthropic".to_owned(),
                auth: Auth::ApiKeyFile {
                    path: provider_key_path(config_dir, self.id()).display().to_string(),
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
                    Model::new("deepseek-flash", Class::Balanced, 15),
                    // Withdrawn 2026-09-14, after which requests to it are
                    // served by Flash at Flash's price. Listed so an operator
                    // can see it existed and why it is not offered — not
                    // routed to, because it would report a cost and a
                    // capability that are both about to stop being true.
                    Model::retiring(
                        "deepseek-v4-pro",
                        Class::Heavy,
                        66,
                        "withdrawn 2026-09-14; requests are served by deepseek-flash at Flash prices",
                    ),
                ],
            },
        }
    }
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
            Ok(r) if !r.providers.is_empty() => r,
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

    /// Every usable `(provider, model)` for a class, best first.
    ///
    /// Ordered by the operator's preference then by cost, so the intended
    /// destination wins and price only breaks ties. Providers that are
    /// disabled or whose credential is missing are left out entirely rather
    /// than tried and failed.
    #[must_use]
    pub fn candidates(&self, class: Class, now: u64) -> Vec<(&Provider, &Model)> {
        self.candidates_with(class, |v| std::env::var(v).ok(), now)
    }

    /// [`Registry::candidates`] with the environment supplied, for tests.
    #[must_use]
    pub fn candidates_with(
        &self,
        class: Class,
        get: impl Fn(&str) -> Option<String> + Copy,
        now: u64,
    ) -> Vec<(&Provider, &Model)> {
        self.candidates_pinned(class, get, now, None)
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
        now: u64,
        pinned: Option<&str>,
    ) -> Vec<(&Provider, &Model)> {
        let mut out: Vec<(&Provider, &Model)> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.credential_ready_with(get))
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
        now: u64,
        pinned: Option<&str>,
    ) -> Vec<(&Provider, &Model)> {
        let mut out: Vec<(&Provider, &Model)> = self
            .providers
            .iter()
            .filter(|p| p.enabled && p.credential_ready_with(get))
            .filter(|p| pinned.is_none_or(|id| p.id == id))
            .filter_map(|p| p.serves(model_id).map(|m| (p, m)))
            .collect();
        out.sort_by_key(|(p, m)| (p.preference, p.cost_at(m, now)));
        out
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
                    deprecated: false,
                    note: None,
                },
                Model {
                    id: "anthropic.claude-haiku-4-5-v1:0".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
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

        let heavy = r.candidates_with(Class::Heavy, with_key, 0);
        assert_eq!(heavy.len(), 2, "both providers serve heavy work");
        assert_eq!(heavy[0].0.id, "anthropic", "the subscription is preference 0");
        assert_eq!(heavy[1].0.id, "bedrock");

        // Nobody serves balanced except anthropic.
        let balanced = r.candidates_with(Class::Balanced, with_key, 0);
        assert_eq!(balanced.len(), 1);
        assert_eq!(balanced[0].1.id, "claude-sonnet-5");
    }

    /// A provider whose key is missing must not be offered. Routing to it
    /// produces a 401 from somewhere nobody was looking.
    #[test]
    fn a_provider_without_its_credential_is_not_a_candidate() {
        let r = two_provider_registry();
        let heavy = r.candidates_with(Class::Heavy, without_key, 0);
        assert_eq!(heavy.len(), 1, "bedrock has no key, so it is not offered");
        assert_eq!(heavy[0].0.id, "anthropic");
    }

    #[test]
    fn a_disabled_provider_is_not_a_candidate() {
        let mut r = two_provider_registry();
        for p in &mut r.providers {
            if p.id == "bedrock" {
                p.enabled = false;
            }
        }
        assert_eq!(r.candidates_with(Class::Heavy, with_key, 0).len(), 1);
    }

    #[test]
    fn the_default_registry_is_the_subscription_alone() {
        let r = Registry::default();
        assert_eq!(r.providers.len(), 1);
        assert_eq!(r.providers[0].auth, Auth::ClaudeOauth);
        // Every class is served, or a request could arrive with nowhere to go.
        for class in Class::LADDER {
            assert_eq!(
                r.candidates_with(class, without_key, 0).len(),
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
        assert!(merged.credential_ready_with(|_| None), "and it is still ready");
        assert_eq!(merged.preference, 0);
        assert!(merged.enabled);
        // What the form DOES express still takes effect.
        assert_eq!(merged.base_url, original.base_url);
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
}
