// SPDX-License-Identifier: MPL-2.0

//! Inspection mode — capture the JSON actually sent to Anthropic.
//!
//! When a request behaves oddly the one thing nobody can see is the request
//! itself. The client builds it, the proxy reshapes it (routing rewrites the
//! model, `anthropic-beta` is merged, the credential is swapped), and what
//! finally goes on the wire exists for a few milliseconds inside a blocking
//! task. This records it.
//!
//! # Why it is off, and why it turns itself off
//!
//! A captured request is a prompt, and a prompt is whatever the person was
//! working on — source, customer data, an API key they pasted in. That is a
//! different class of thing from the usage counters beside it, so:
//!
//! * **off by default**, armed explicitly for a stated number of minutes;
//! * **it disarms itself** when that runs out. A debug switch left on is how a
//!   week of everyone's prompts ends up in a file nobody remembers, and
//!   "remember to turn it off" is not a control;
//! * **bounded** — [`KEEP`] captures, each truncated to [`MAX_BODY`];
//! * **0600**, beside the accounts, and never served to a user key.
//!
//! # What is removed before anything is written
//!
//! Two credentials pass through this path and neither may be recorded:
//!
//! * the **proxy's** Claude OAuth token, in the outgoing `Authorization`;
//! * the **caller's** user key, in the incoming `x-api-key`.
//!
//! Headers are handled by allowlist — anything not named is dropped rather
//! than kept — and the bodies are additionally swept for token-shaped strings,
//! because a prompt can contain a credential that no header rule would catch
//! (someone pasting their own key in to ask why it is failing is precisely the
//! kind of session that gets inspected).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tab_atelier_api::schemars;

use crate::users::Client;

/// How many captures to keep.
///
/// Small on purpose: this is for looking at the last few calls, not for building
/// an archive. With full bodies now stored rather than clipped fragments, this is
/// the number that bounds the file.
pub const KEEP: usize = 25;

/// Longest body recorded, in bytes.
///
/// This started at 32 KB, on the theory that a truncated request still showed
/// the interesting parts. It does not: a body cut in half is not JSON, so the
/// one thing anybody opens this panel to do — read the object that was sent —
/// was impossible for essentially every real request. A Claude Code call with
/// a working context is a few hundred KB, so the cap is now well clear of
/// real traffic and exists only so that one pathological request cannot fill
/// the disk.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// How much of a reply is kept on the capture.
///
/// Much smaller than [`MAX_BODY`]: a request is kept to be read in full, but a
/// reply is read to answer "what did it say", and the answer is at the front.
/// The cap also holds for a streamed reply, which arrives in pieces and would
/// otherwise accumulate every token of a long generation.
pub const MAX_EXCERPT: usize = 8 * 1024;

/// Longest an inspection window may be armed for.
///
/// A cap rather than a suggestion: the reason this exists is that a switch
/// left on indefinitely accumulates other people's prompts, and an operator
/// typing a large number is exactly how that happens.
pub const MAX_ARM_MINUTES: u64 = 60;

/// Headers worth keeping on a capture.
///
/// An allowlist. The two that must never appear — `authorization` and
/// `x-api-key` — are absent by construction rather than by a rule that could
/// be forgotten when a header is added.
const KEEP_HEADERS: &[&str] = &[
    "content-type",
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
    "x-app",
    "x-claude-code-session-id",
    "accept",
];

/// One recorded call.
#[derive(Debug, Clone, Serialize, Deserialize, tab_atelier_api::schemars::JsonSchema)]
pub struct Capture {
    /// Which capture this is, within the ring the process is holding.
    ///
    /// Assigned by [`Store`], never by the caller, and it exists because the
    /// panel now lists captures with their bodies withheld and asks for one at
    /// a time. A position in the ring would not do: one call arriving between
    /// the list and the click shifts every position after it, so the row the
    /// operator clicked would open a different request.
    ///
    /// Meaningless across restarts, which is why [`Store::load`] renumbers
    /// rather than trusting what the file says — a record written by a build
    /// that had this field and one written by a build that did not would
    /// otherwise need two schemes for one job.
    #[serde(default)]
    pub id: u64,
    /// RFC3339.
    pub ts: String,
    pub account_id: String,
    pub account_email: String,
    pub method: String,
    /// Path and query as sent upstream.
    pub path: String,
    /// Where it went, after routing.
    pub provider: String,
    /// The model actually requested upstream, which is not necessarily the one
    /// the client asked for — that is half the point of looking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Which client session sent this, from the body's `metadata`.
    ///
    /// The one field on a capture that answers "who" at a finer grain than the
    /// key. A key belongs to a PLACE, and one machine runs many tabs; without
    /// this, forty calls from four sessions are one undifferentiated row.
    ///
    /// Client-supplied and NOT authenticated: anything holding a valid key can
    /// put any string here, so it groups and explains, and must never be the
    /// basis of a billing or access decision. See [`Client`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<Client>,
    /// The server-side tool this call is, when it is one.
    ///
    /// A web search is not work the operator asked for directly: the model asks
    /// for it, and Claude Code answers by making another `/v1/messages` call
    /// whose only tool is `web_search`. In a list of a hundred calls that turn
    /// is otherwise indistinguishable from a conversation — same key, same
    /// model, plausible token count — so it is labelled.
    ///
    /// The family only, without the version date: see [`server_tool`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool: Option<String>,
    /// Whether this is the conversation or the auto-mode permission classifier.
    ///
    /// Defaults to `Work`, which is the right reading for the lines already on
    /// disk and for any request the detector does not fire on. The panel uses
    /// it to keep the judge out of the per-account call and token figures,
    /// which answer a question about work.
    #[serde(default)]
    pub kind: crate::classifier::Kind,
    pub request_headers: Vec<(String, String)>,
    /// The JSON sent to Anthropic, scrubbed and truncated.
    pub request_body: String,
    /// True when [`MAX_BODY`] cut it. Stated, so nobody debugs a JSON parse
    /// error caused by our own truncation.
    pub request_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// What upstream said this call cost, read off the response.
    ///
    /// The same numbers the billing path uses — taken from the one parse, not
    /// counted a second time. Without them the panel shows what was SENT and
    /// says nothing about what it cost, which is half of why anybody opens it:
    /// "why was that turn expensive" is answered by these four numbers beside
    /// the request that produced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<crate::usage::Tokens>,
    /// What those tokens cost, in the same shape the usage ledger stores.
    ///
    /// The same numbers, from the same computation, as the ones folded into the
    /// hour bucket — `relay::charge` is called once and its result is used by
    /// both. Recording it rather than re-deriving it in the panel is the rule
    /// the ledger already follows: a figure worked out at read time is
    /// multiplied by whatever the rate table says *now*, so a capture from last
    /// month would be re-priced at today's rates.
    ///
    /// `None` when the round's price is unknown — an unpriced model, or a tool
    /// call on a hop with no tool price — because a missing price is not a zero
    /// price there either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<crate::usage::Cost>,
    /// The rate those tokens were priced at, per 1M, with any peak window
    /// already applied.
    ///
    /// Beside the cost because the panel prices individual blocks, and it must
    /// use the rate the request was actually billed at. The client cannot
    /// recompute that: `Price` is the *base* triple and applying a peak window
    /// is arithmetic the server owns (see `Provider::price_at`), so a panel
    /// that multiplied by the exposed price would be wrong by the peak
    /// multiplier for half of every weekday on hops that have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<crate::provider::Price>,
    /// The dialect this request left on, as a plain string.
    ///
    /// `None` in a capture written before the field existed, which the
    /// conversation view reads as "unknown" and says nothing about rather than
    /// assuming Anthropic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<String>,
    /// What compaction removed from this request, when it ran.
    ///
    /// Otherwise invisible: the panel shows the body AS SENT, so a compacted
    /// request simply looks like a smaller one. Without this, "is compaction
    /// on, and is it doing anything" can only be answered by comparing against
    /// a provider that has it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,
    /// How the request arrived — see [`Origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// The head of the reply, as the client received it.
    ///
    /// Streamed replies are the raw SSE frames, not a JSON document, so this is
    /// kept verbatim and shown verbatim. It exists to separate "the request was
    /// rejected" from "the model answered with nothing", which look identical
    /// from the request side alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_excerpt: Option<String>,
    /// True when [`MAX_EXCERPT`] cut the reply. Stated for the same reason as
    /// [`Self::request_truncated`]: a clipped reply is not a short one.
    #[serde(default)]
    pub response_truncated: bool,
}

/// What one compaction pass removed, recorded on the capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, tab_atelier_api::schemars::JsonSchema)]
pub struct Compaction {
    /// The level that ran, as `providers.json` spells it.
    pub level: String,
    /// The request body before and after. Both numbers rather than a
    /// percentage: the ratio is arithmetic the reader can do, and a stale
    /// percentage is harder to notice than two sizes that do not look right.
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub tool_results_elided: usize,
    /// Tool results left alone because they carry an error — see
    /// [`crate::compact`]. Reported so a pass that elided less than expected
    /// is explicable rather than mysterious.
    pub tool_results_kept_for_error: usize,
    /// Tool results a stub would have lengthened — kept verbatim.
    pub tool_results_kept_small: usize,
    /// Tool results the pass found and *declined* to elide, because the stale
    /// region was under [`crate::compact`]'s `ELIDE_ABOVE_BYTES` floor.
    ///
    /// The same reason as the field above it, for a failure that is harder to
    /// see. A transcript the panel shows as repeatedly compacted with nothing
    /// saved is `0` here when there was genuinely nothing to do, and nonzero
    /// when elision was available and refused — and those want opposite
    /// investigations. On 2026-09-25 the refused case was the live one: a client
    /// looped on stubs the pass had written into a body it had no contextual
    /// reason to touch, and nothing in the record said so.
    ///
    /// Deserialization defaulted, unlike its neighbours: captures are written to
    /// disk, so a record from a proxy that predates this field has to keep
    /// loading. `0` is also the truthful answer for those — the pass that wrote
    /// them had no floor and so declined nothing.
    #[serde(default)]
    pub tool_results_kept_under_budget: usize,
    pub thinking_dropped: usize,
    pub notices_dropped: usize,
}

impl Compaction {
    /// Bytes shed. Saturating, so a pass that somehow grew the body reports
    /// nothing saved rather than wrapping.
    #[must_use]
    pub const fn saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Replace credential-shaped runs with a marker.
///
/// Belt to the header allowlist's braces. Anthropic tokens are
/// `sk-ant-…` and this proxy's keys are `tap_…`; both are long unbroken runs
/// of URL-safe characters, so they are recognisable without knowing the value.
/// A prompt that contains one — someone pasting a key in to ask why it does
/// not work — must not turn a debugging aid into a second place the key lives.
#[must_use]
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("sk-ant-").or_else(|| rest.find("tap_")) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let end = tail
            .char_indices()
            .position(|(_, c)| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(tail.len());
        out.push_str("<redacted-credential>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The nearest char boundary at or before `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    (0..=index.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)
}

/// The nearest char boundary at or after `index`.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let index = index.min(text.len());
    (index..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len())
}

/// What takes the place of text cut out of a string value.
fn elide_notice(bytes: usize) -> String {
    format!("[{bytes} bytes elided by tab-atelier-proxy]")
}

/// Cap every string value, keeping at least `head` bytes at its start and
/// `tail` at its end. Returns how many bytes were removed.
///
/// Inside the strings, never across the document: see [`clamp`].
fn clamp_strings(value: &mut serde_json::Value, head: usize, tail: usize) -> usize {
    let mut elided = 0;
    match value {
        // The guard is part of the pattern so the match stays one level deep.
        serde_json::Value::String(text) if text.len() > head + tail => {
            let head_end = floor_boundary(text, head);
            let tail_start = ceil_boundary(text, text.len() - tail);
            if tail_start > head_end {
                elided += tail_start - head_end;
                let notice = elide_notice(tail_start - head_end);
                *text = format!("{}{}{}", &text[..head_end], notice, &text[tail_start..]);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                elided += clamp_strings(item, head, tail);
            }
        }
        serde_json::Value::Object(fields) => {
            for (_, field) in fields.iter_mut() {
                elided += clamp_strings(field, head, tail);
            }
        }
        _ => {}
    }
    elided
}

/// Keep the first and last `keep` entries of every long array, replacing the
/// middle with one marker object. Returns how many entries were removed.
///
/// The last structural resort, for a JSON body made of so many entries that
/// shrinking their text still does not fit. Whole blocks go rather than the
/// document's shape: a body that no longer parses would show nothing at all,
/// while this one still shows the start and the end of the request, which is
/// what someone diagnosing a bill is looking for.
fn clamp_arrays(value: &mut serde_json::Value, keep: usize) -> usize {
    let mut dropped = 0;
    match value {
        serde_json::Value::Array(items) => {
            if items.len() > keep * 2 + 1 {
                let tail_start = items.len() - keep;
                let removed = tail_start - keep;
                // Two disjoint slices, so split rather than chain two borrows.
                let (head, tail) = items.split_at_mut(tail_start);
                for item in head.iter_mut().chain(tail.iter_mut()) {
                    dropped += clamp_arrays(item, keep);
                }
                let marker = serde_json::json!({
                    "type": "text",
                    "text": elide_notice(removed),
                });
                items.splice(keep..tail_start, std::iter::once(marker));
                dropped += removed;
            } else {
                for item in items.iter_mut() {
                    dropped += clamp_arrays(item, keep);
                }
            }
        }
        serde_json::Value::Object(fields) => {
            for (_, field) in fields.iter_mut() {
                dropped += clamp_arrays(field, keep);
            }
        }
        _ => {}
    }
    dropped
}

/// Keep both ends of an over-long body.
///
/// This used to slice the raw bytes, which left a body that was not JSON: the
/// panel could not parse it, so the one thing a capture exists for — reading the
/// request that was sent — was impossible for exactly the requests worth
/// reading. Large text is now cut *inside its string values* instead, so the
/// document stays valid, every block boundary survives, and the figures derived
/// from those blocks stay comparable. Only the middle of the longest text is
/// replaced, and the notice says how much.
fn clamp(body: &str) -> (String, bool) {
    if body.len() <= MAX_BODY {
        return (body.to_owned(), false);
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        // Shrink the per-string allowance until the document fits. Each pass
        // parses nothing and serialises once, and only a pathological body needs
        // more than a couple; the floor guarantees termination.
        //
        // The floor is low on purpose: a long transcript is thousands of *short*
        // text blocks rather than one huge one, so a 256-byte allowance would cut
        // nothing and the body would fall through to the raw path, losing its
        // structure after all.
        let mut allowance = MAX_BODY / 8;
        while allowance >= 32 {
            let mut candidate = value.clone();
            let elided = clamp_strings(&mut candidate, allowance, allowance);
            let text = candidate.to_string();
            if elided > 0 && text.len() <= MAX_BODY {
                return (text, true);
            }
            allowance /= 4;
        }

        // Still too large: thousands of entries whose text is already short.
        // Drop the middle of the long arrays rather than the document's shape.
        for keep in [256usize, 32, 4] {
            let mut candidate = value.clone();
            if clamp_strings(&mut candidate, 64, 64) == 0 && clamp_arrays(&mut candidate, keep) == 0 {
                continue;
            }
            let text = candidate.to_string();
            if text.len() <= MAX_BODY {
                return (text, true);
            }
        }
    }

    clamp_raw(body)
}

/// The last resort, for a body that is not JSON or cannot be shrunk: keep both
/// ends. The result will not parse, which is why it is not the first choice.
fn clamp_raw(body: &str) -> (String, bool) {
    if body.len() <= MAX_BODY {
        return (body.to_owned(), false);
    }
    let head_end = floor_boundary(body, MAX_BODY / 2);
    let tail_start = ceil_boundary(body, body.len() - MAX_BODY / 2);
    (
        format!(
            "{}\n\n… {} bytes omitted by tab-atelier-proxy …\n\n{}",
            &body[..head_end],
            body.len() - head_end - (body.len() - tail_start),
            &body[tail_start..]
        ),
        true,
    )
}

/// A request on its way upstream, as [`capture`] needs to see it.
///
/// A struct rather than eight parameters: they are all strings, and eight
/// positional strings is a call nobody can read and a swap nobody notices.
pub struct Outgoing<'a> {
    pub ts: String,
    pub account_id: &'a str,
    pub account_email: &'a str,
    pub method: &'a str,
    /// Path and query, as sent.
    pub path: &'a str,
    pub provider: &'a str,
    /// The conversation, or the auto-mode classifier. Detected from the body
    /// by the caller, which has already parsed it to route the request.
    pub kind: crate::classifier::Kind,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    /// What the request looked like on ARRIVAL, when the caller knows.
    ///
    /// The rest of a capture is the request as it leaves, which is the half
    /// nobody can otherwise see. This is the other end of the same question,
    /// and it is here because a client-IP question took three exchanges to
    /// even state: every answer was about the code, and the code was reading
    /// the header it was asked to read. What was missing was the value that
    /// actually arrived, and the peer it arrived from.
    pub origin: Option<Origin>,
    /// The dialect the request left on.
    ///
    /// Recorded rather than looked up later because it is what decides whether
    /// the `cache_control` breakpoints in the captured body mean anything: on
    /// the Anthropic wire they are honoured, and on the `OpenAI` wire they are
    /// stripped on the way out because that endpoint prefix-caches implicitly.
    /// A reader comparing breakpoints to a bill needs to know which of the two
    /// they are looking at, and the provider's configuration can be changed
    /// after the capture was taken.
    pub wire: crate::provider::Wire,
}

/// Unpack `metadata.user_id`, if the body has one.
///
/// The parse itself lives in [`crate::users::Client::from_body`], because a
/// capture is no longer the only place that wants it: a key remembers the
/// sessions it has seen, and that must happen whether or not a capture is
/// armed. One implementation, so the two cannot disagree about an all-empty
/// `metadata`.
fn parse_client(body: &serde_json::Value) -> Option<Client> {
    Client::from_body(body)
}

/// The server-side tool a request asks for, if it asks for one.
///
/// Anthropic's own tools are versioned TYPES — `web_search_20250305`,
/// `web_fetch_20250910` — while a caller's own tool has no `type` at all, only
/// a name and an input schema. So the version suffix is the discriminator, and
/// it is one a client cannot fake by naming its tool `web_search`: without the
/// date there is no type field, and with one it is indistinguishable from the
/// real thing, which is the budget the label cares about anyway.
///
/// The date is stripped so the label survives the next revision. Anthropic
/// ships a new date when the tool changes; hardcoding today's would leave the
/// badge silently absent the day `web_search_20260209` arrives, which is the
/// exact failure a label is supposed to make visible.
pub(crate) fn server_tool(body: &serde_json::Value) -> Option<String> {
    body.get("tools")?
        .as_array()?
        .iter()
        .filter_map(|tool| tool.get("type")?.as_str())
        .find_map(|ty| {
            let (family, date) = ty.rsplit_once('_')?;
            let versioned = date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit());
            (versioned && !family.is_empty()).then(|| family.to_owned())
        })
}

/// Where a request came from, as the proxy saw it.
///
/// Both the resolved answer and the raw inputs, deliberately. Showing only the
/// resolved IP answers "what did we record" and not "why" — and the why is the
/// whole question when it is wrong.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, tab_atelier_api::schemars::JsonSchema)]
pub struct Origin {
    /// The socket peer, before any header is believed.
    pub peer: String,
    /// Whether that peer was trusted enough to read a forwarding header at all.
    /// `false` explains a result that ignored a header the operator can see.
    pub peer_trusted: bool,
    /// What the proxy resolved, and therefore what it recorded against a key.
    pub client_ip: String,
    /// The headers as received, including whether they were there at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_real_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_forwarded_for: Option<String>,
}

/// Build a capture from the request as it is about to leave.
#[must_use]
pub fn capture(o: &Outgoing<'_>) -> Capture {
    let Outgoing {
        ts,
        account_id,
        account_email,
        method,
        path,
        provider,
        kind,
        headers,
        body,
        // Not destructured: read straight off `o` at the end, so adding a
        // field to `Outgoing` does not mean touching this binding list too.
        ..
    } = o;
    let raw = String::from_utf8_lossy(body);
    let scrubbed = scrub(&raw);
    let parsed = serde_json::from_str::<serde_json::Value>(&raw).ok();
    let model = parsed
        .as_ref()
        .and_then(|v| v.get("model").and_then(serde_json::Value::as_str).map(str::to_owned));
    let client = parsed.as_ref().and_then(parse_client);
    let server_tool = parsed.as_ref().and_then(server_tool);
    let (request_body, request_truncated) = clamp(&scrubbed);
    Capture {
        // The store's to give: one capture has no identity until it is in the
        // ring, and a caller that made one up could collide with a live one.
        id: 0,
        ts: ts.clone(),
        account_id: (*account_id).to_owned(),
        account_email: (*account_email).to_owned(),
        method: (*method).to_owned(),
        path: (*path).to_owned(),
        provider: (*provider).to_owned(),
        model,
        client,
        server_tool,
        kind: *kind,
        request_headers: headers
            .iter()
            .filter(|(k, _)| KEEP_HEADERS.contains(&k.to_ascii_lowercase().as_str()))
            .map(|(k, v)| (k.clone(), scrub(v)))
            .collect(),
        request_body,
        request_truncated,
        response_truncated: false,
        status: None,
        tokens: None,
        // Priced by the caller, which is where the rate table and the response
        // meet. See `relay::finish_capture`.
        cost: None,
        rate: None,
        // Named, not derived: `Wire` has no `Serialize`, and the string is what
        // the panel compares against.
        wire: Some(
            match o.wire {
                crate::provider::Wire::Anthropic => "anthropic",
                crate::provider::Wire::Openai => "openai",
            }
            .to_owned(),
        ),
        // Filled in by the caller, which is the only thing that knows whether
        // a compaction pass ran on this request at all.
        compaction: None,
        origin: o.origin.clone(),
        response_excerpt: None,
    }
}

/// The armed window and the ring of captures.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    /// Unix seconds until which capturing is on. Zero is off.
    armed_until: u64,
    /// The id the next capture gets. Only has to be unique among the captures
    /// held right now, which is why [`Self::load`] restarts the count rather
    /// than continuing from whatever the file says.
    next_id: u64,
    recent: Vec<Capture>,
}

impl Store {
    /// Load previous captures, if any. Never arms itself on load: an armed
    /// window does not survive a restart, because the operator who armed it is
    /// not necessarily the one who restarted the service.
    #[must_use]
    pub fn load(dir: impl Into<PathBuf>) -> Self {
        let path = dir.into().join("inspect.jsonl");
        let mut recent = std::fs::read_to_string(&path).map_or_else(
            |_| Vec::new(),
            |raw| {
                raw.lines()
                    .filter_map(|l| serde_json::from_str::<Capture>(l).ok())
                    .rev()
                    .take(KEEP)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            },
        );
        // Renumbered rather than trusted: a file written before this field
        // existed carries `id: 0` on every line, and several captures sharing
        // one id would make the panel's "open the one I clicked" a guess.
        let mut next_id = 1;
        for c in &mut recent {
            c.id = next_id;
            next_id += 1;
        }
        Self {
            path,
            armed_until: 0,
            next_id,
            recent,
        }
    }

    /// Turn capture on for `minutes`, capped at [`MAX_ARM_MINUTES`].
    /// Returns the deadline.
    pub fn arm(&mut self, now: u64, minutes: u64) -> u64 {
        self.armed_until = now + minutes.clamp(1, MAX_ARM_MINUTES) * 60;
        self.armed_until
    }

    pub const fn disarm(&mut self) {
        self.armed_until = 0;
    }

    /// Whether a call happening now should be recorded.
    #[must_use]
    pub const fn armed(&self, now: u64) -> bool {
        self.armed_until > now
    }

    #[must_use]
    pub const fn armed_until(&self) -> u64 {
        self.armed_until
    }

    #[must_use]
    pub fn recent(&self) -> &[Capture] {
        &self.recent
    }

    /// The capture with this id, while the ring still holds it.
    ///
    /// `None` is an ordinary answer, not an error: the ring turns over, so a
    /// row a browser listed a minute ago can name a capture that a newer one
    /// has already displaced.
    #[must_use]
    pub fn find(&self, id: u64) -> Option<&Capture> {
        self.recent.iter().find(|c| c.id == id)
    }

    /// Forget everything captured so far, on disk as well as in memory.
    ///
    /// # Errors
    /// The file exists and could not be removed.
    pub fn clear(&mut self) -> Result<(), String> {
        self.recent.clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", self.path.display())),
        }
    }

    /// Record one call, trimming to [`KEEP`].
    pub fn push(&mut self, mut c: Capture) {
        c.id = self.next_id;
        self.next_id += 1;
        self.recent.push(c);
        let overflow = self.recent.len().saturating_sub(KEEP);
        if overflow > 0 {
            self.recent.drain(..overflow);
        }
        // Rewritten whole rather than appended: the ring has to be trimmed on
        // disk too, or a long window leaves every prompt of the session in a
        // file the bound implied was small.
        let _ = self.persist();
    }

    fn persist(&self) -> Result<(), String> {
        use std::io::Write as _;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Prompts. Not world-readable, from creation.
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("open {}: {e}", tmp.display()))?;
        for c in &self.recent {
            let line = serde_json::to_string(c).map_err(|e| e.to_string())?;
            writeln!(f, "{line}").map_err(|e| e.to_string())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    /// The real thing, copied from a live request body.
    ///
    /// Reproduced rather than invented, because the whole difficulty of this
    /// field is that it is a JSON document stored inside a JSON string — a
    /// fixture written from memory would likely be the shape someone WISHED
    /// the wire had.
    #[test]
    fn the_client_identity_unpacks_from_its_own_nested_json() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"model":"claude-opus-5-5","metadata":{"user_id":"{\"device_id\":\"323056b1018ba40f8d9327977429a92eab0a05a7a6a1a8d8f026bef3b779d625\",\"account_uuid\":\"\",\"session_id\":\"a3412ddb-82d5-4ce5-868a-1c8475b5cb38\"}"}}"#,
        )
        .expect("parses");

        let client = parse_client(&body).expect("a client");
        assert_eq!(
            client.device_id.as_deref(),
            Some("323056b1018ba40f8d9327977429a92eab0a05a7a6a1a8d8f026bef3b779d625")
        );
        assert_eq!(
            client.session_id.as_deref(),
            Some("a3412ddb-82d5-4ce5-868a-1c8475b5cb38")
        );
        // `account_uuid: ""` is what a logged-out session sends. Reported as
        // ABSENT, because rendering it would state something the wire did not.
        assert_eq!(client.account_uuid, None);
    }

    /// Everything absent, or absent entirely, is `None` — not an empty struct.
    /// A capture carrying `{"client":{}}` says "there is a client and it told
    /// us nothing", which is a different and wrong claim.
    #[test]
    fn a_body_without_a_usable_user_id_has_no_client() {
        for body in [
            json!({"model": "claude-opus-5-5"}),
            json!({"metadata": {}}),
            json!({"metadata": {"user_id": ""}}),
            json!({"metadata": {"user_id": "not json at all"}}),
            json!({"metadata": {"user_id": "[1,2,3]"}}),
            json!({"metadata": {"user_id": 42}}),
            json!({"metadata": "user_id"}),
            // Parses, but names nothing we can report.
            json!({"metadata": {"user_id": "{\"account_uuid\":\"\"}"}}),
            json!({"metadata": {"user_id": "{}"}}),
        ] {
            assert_eq!(parse_client(&body), None, "should be no client: {body}");
        }
    }

    /// A partial payload still reports the parts it has — the fields are
    /// independent, and a client that sends only a session id is usable.
    #[test]
    fn a_partial_user_id_reports_what_it_has() {
        let body = json!({"metadata": {"user_id": "{\"session_id\":\"s-1\"}"}});
        let client = parse_client(&body).expect("a client");
        assert_eq!(client.session_id.as_deref(), Some("s-1"));
        assert_eq!(client.device_id, None);

        // And non-string members are ignored rather than coerced.
        let body = json!({"metadata": {"user_id": "{\"session_id\":42,\"device_id\":\"d-1\"}"}});
        let client = parse_client(&body).expect("a client");
        assert_eq!(client.session_id, None);
        assert_eq!(client.device_id.as_deref(), Some("d-1"));
    }

    /// The field is optional on capture, so a file written before it existed
    /// still parses — the same rule every other added field here follows.
    #[test]
    fn a_capture_without_a_client_reads_back_as_none() {
        let old = r#"{"ts":"2026-09-11T00:00:00Z","account_id":"a","account_email":"e",
                      "method":"POST","path":"/v1/messages","provider":"anthropic",
                      "request_headers":[],"request_body":"{}","request_truncated":false}"#;
        let parsed: Capture = serde_json::from_str(old).expect("an older capture must load");
        assert_eq!(parsed.client, None);
        // And it is not written back out as a null, which would grow every
        // stored line for no information.
        let round = serde_json::to_string(&parsed).expect("serializes");
        assert!(!round.contains("client"), "{round}");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ta-inspect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn neither_credential_on_this_path_can_reach_the_file() {
        // The two that pass through: the proxy's Claude token and the caller's
        // user key. A capture that recorded either would turn a debugging aid
        // into a second place a live credential lives.
        let body = r#"{"model":"claude-opus-5-5","system":"my key is sk-ant-oat01-AAAABBBBCCCC and the proxy one is tap_deadbeefcafe0123"}"#;
        let out = scrub(body);
        assert!(!out.contains("sk-ant-oat01-AAAABBBBCCCC"), "{out}");
        assert!(!out.contains("tap_deadbeefcafe0123"), "{out}");
        assert_eq!(out.matches("<redacted-credential>").count(), 2, "{out}");
        // Everything around them survives — a scrub that ate the request would
        // defeat the purpose.
        assert!(out.contains(r#""model":"claude-opus-5-5""#), "{out}");
        assert!(out.contains("my key is"), "{out}");
    }

    #[test]
    fn the_authorization_header_is_absent_by_construction() {
        // Headers are an allowlist, so a credential is not dropped by a rule
        // that someone could forget to update — it is simply never named.
        let headers = vec![
            ("Authorization".to_owned(), "Bearer sk-ant-oat01-LIVE".to_owned()),
            ("x-api-key".to_owned(), "tap_callerkey".to_owned()),
            ("CF-Access-Client-Secret".to_owned(), "cf-secret".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
        ];
        let c = capture(&Outgoing {
            ts: "2026-09-10T12:00:00Z".to_owned(),
            account_id: "id",
            account_email: "ada@example.org",
            method: "POST",
            path: "/v1/messages",
            provider: "anthropic",
            kind: crate::classifier::Kind::Work,
            headers: &headers,
            origin: None,
            wire: crate::provider::Wire::Anthropic,
            body: br#"{"model":"claude-haiku-5-5"}"#,
        });
        let names: Vec<_> = c.request_headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(!names.contains(&"authorization".to_owned()), "{names:?}");
        assert!(!names.contains(&"x-api-key".to_owned()), "{names:?}");
        assert!(!names.contains(&"cf-access-client-secret".to_owned()), "{names:?}");
        assert!(names.contains(&"content-type".to_owned()));
        let serialised = serde_json::to_string(&c).expect("serialise");
        assert!(!serialised.contains("sk-ant-oat01-LIVE"), "{serialised}");
        assert!(!serialised.contains("tap_callerkey"), "{serialised}");
        assert!(!serialised.contains("cf-secret"), "{serialised}");
        // The model upstream was asked for is pulled out for the list view.
        assert_eq!(c.model.as_deref(), Some("claude-haiku-5-5"));
    }

    #[test]
    fn an_armed_window_expires_on_its_own_and_cannot_be_armed_forever() {
        let mut s = Store::load(tmp("arm"));
        assert!(!s.armed(1000), "off by default — prompts are not collected unasked");

        let deadline = s.arm(1000, 15);
        assert_eq!(deadline, 1000 + 15 * 60);
        assert!(s.armed(1000));
        assert!(s.armed(1000 + 14 * 60));
        // The whole point: nobody has to remember to switch it off.
        assert!(!s.armed(1000 + 15 * 60), "the window closes by itself");

        // A large number is clamped rather than honoured.
        let deadline = s.arm(0, 10_000);
        assert_eq!(
            deadline,
            MAX_ARM_MINUTES * 60,
            "an unbounded window is the failure mode"
        );
    }

    #[test]
    fn captures_are_bounded_and_land_unreadable_to_anyone_else() {
        let dir = tmp("ring");
        let mut s = Store::load(&dir);
        let one = |n: usize| {
            capture(&Outgoing {
                ts: format!("2026-09-10T12:00:{n:02}Z"),
                account_id: "id",
                account_email: "ada@example.org",
                method: "POST",
                path: "/v1/messages",
                provider: "anthropic",
                kind: crate::classifier::Kind::Work,
                headers: &[],
                origin: None,
                wire: crate::provider::Wire::Anthropic,
                body: br#"{"model":"m"}"#,
            })
        };
        for n in 0..KEEP + 10 {
            s.push(one(n));
        }
        assert_eq!(s.recent().len(), KEEP, "the ring must not grow without bound");
        // Trimmed on disk too, not just in memory.
        let lines = std::fs::read_to_string(s.path()).expect("read").lines().count();
        assert_eq!(lines, KEEP, "the file must be bounded as well");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(s.path()).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "captured prompts must not be world-readable");
        }

        // Reload keeps the captures but NOT the armed window: whoever restarts
        // the service has not consented to keep collecting.
        s.arm(1000, 30);
        let back = Store::load(&dir);
        assert_eq!(back.recent().len(), KEEP);
        assert!(!back.armed(1000), "an armed window must not survive a restart");

        let mut back = back;
        back.clear().expect("clear");
        assert!(back.recent().is_empty());
        assert!(!back.path().exists(), "clearing removes the file, not just the memory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture whose only distinguishing part is its timestamp.
    ///
    /// The store's tests below assert on ids and on the ring's bounds, so they
    /// need a capture and nothing about the caller: the account is deliberately
    /// left blank rather than carrying a plausible-looking address into a
    /// fixture that never reads one.
    fn fixture_capture(n: usize) -> Capture {
        capture(&Outgoing {
            ts: format!("2026-09-10T12:00:{n:02}Z"),
            account_id: "id",
            account_email: "",
            method: "POST",
            path: "/v1/messages",
            provider: "anthropic",
            kind: crate::classifier::Kind::Work,
            headers: &[],
            origin: None,
            wire: crate::provider::Wire::Anthropic,
            body: br#"{"model":"m"}"#,
        })
    }

    /// The panel lists captures with their bodies withheld and asks for one at a
    /// time, so a row has to name a capture that a later request can still find.
    /// Position in the ring cannot do that: it shifts as soon as the next call is
    /// recorded, and the row the operator clicked would open someone else's.
    #[test]
    fn every_capture_gets_an_id_a_row_can_name_it_by() {
        let dir = tmp("ids");
        let mut s = Store::load(&dir);
        for n in 0..3 {
            s.push(fixture_capture(n));
        }

        let ids: Vec<u64> = s.recent().iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        let unique: BTreeSet<u64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "two captures sharing an id is a row opening the wrong one"
        );

        // The id resolves back to the right capture, which is the whole purpose.
        let second = ids[1];
        assert_eq!(s.find(second).map(|c| c.ts.as_str()), Some("2026-09-10T12:00:01Z"));
        assert!(s.find(9_999).is_none(), "an id the ring never handed out is absent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture the ring has displaced is gone, and the panel has to say so
    /// rather than answer with a different request.
    #[test]
    fn a_displaced_capture_cannot_be_found_by_its_old_id() {
        let dir = tmp("displaced");
        let mut s = Store::load(&dir);
        s.push(fixture_capture(0));
        let first = s.recent()[0].id;
        assert!(s.find(first).is_some());

        // Enough further captures to push the first out of the ring.
        for n in 1..=KEEP {
            s.push(fixture_capture(n % 60));
        }

        assert!(s.find(first).is_none(), "a displaced capture must not resolve");
        // And the ids still in the ring are the later ones, each still unique.
        let ids: Vec<u64> = s.recent().iter().map(|c| c.id).collect();
        let unique: BTreeSet<u64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(!ids.contains(&first));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture written before the id existed reads back as `0` on every line.
    /// Loading has to renumber, or several captures would share one id and the
    /// panel's "open the one I clicked" becomes a guess.
    #[test]
    fn a_reloaded_store_renumbers_the_captures_it_reads_back() {
        let dir = tmp("renumber");
        let mut s = Store::load(&dir);
        for n in 0..3 {
            s.push(fixture_capture(n));
        }

        // Rewrite the file as an older build would have left it: no id at all.
        let path = s.path().to_path_buf();
        let rewritten: String = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).expect("parse");
                value["id"] = serde_json::Value::from(0);
                value.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{rewritten}\n")).expect("write");

        let mut back = Store::load(&dir);
        let ids: Vec<u64> = back.recent().iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3, "the captures still load");
        let unique: BTreeSet<u64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "and they must not all share the id they were written with"
        );

        // A capture recorded after the reload cannot collide with them either.
        back.push(fixture_capture(9));
        let ids: Vec<u64> = back.recent().iter().map(|c| c.id).collect();
        let unique: BTreeSet<u64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_enormous_body_is_cut_inside_its_strings_and_still_parses() {
        // What a capture is for is reading the request that was sent, so a body
        // over the cap has to stay readable. Slicing it as raw bytes did not:
        // the result was not JSON, the panel could not parse it, and the one
        // request worth looking at was the one it could not show.
        let big = format!(
            r#"{{"model":"claude-opus-5-5","messages":[{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}]}}"#,
            "x".repeat(MAX_BODY + 1024)
        );
        let c = capture(&Outgoing {
            ts: "t".to_owned(),
            account_id: "id",
            account_email: "",
            method: "POST",
            path: "/v1/messages",
            provider: "anthropic",
            kind: crate::classifier::Kind::Work,
            headers: &[],
            origin: None,
            wire: crate::provider::Wire::Anthropic,
            body: big.as_bytes(),
        });
        assert!(c.request_truncated, "truncation must be declared, not silent");
        assert!(c.request_body.len() <= MAX_BODY);

        // The whole point: it parses, so the structure survives.
        let parsed: serde_json::Value =
            serde_json::from_str(&c.request_body).expect("a clamped body must still be JSON");
        assert_eq!(parsed["model"], "claude-opus-5-5");
        assert_eq!(parsed["messages"][0]["role"], "user");
        let text = parsed["messages"][0]["content"][0]["text"]
            .as_str()
            .expect("the text block is still a string");
        assert!(text.contains("bytes elided by tab-atelier-proxy"));
        assert!(text.starts_with('x'), "the head of the text survives");
        assert!(text.ends_with('x'), "and so does its tail");
    }

    /// A long transcript is thousands of short text blocks, not one huge one, so
    /// an allowance that only cuts long strings would elide nothing and the body
    /// would lose its structure anyway.
    #[test]
    fn a_body_of_many_short_strings_still_parses_after_clamping() {
        let blocks: Vec<String> = (0..40_000)
            .map(|n| format!(r#"{{"role":"user","content":[{{"type":"text","text":"block number {n} with enough text to be worth eliding"}}]}}"#))
            .collect();
        let big = format!(r#"{{"model":"m","messages":[{}]}}"#, blocks.join(","));
        assert!(big.len() > MAX_BODY, "the fixture must exceed the cap");

        let (out, cut) = clamp(&big);
        assert!(cut);
        assert!(out.len() <= MAX_BODY, "got {} bytes", out.len());
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("the structure must survive even when every string is short");
        // The first and last messages survive; the middle is replaced by a
        // marker, because that many entries cannot fit however short their text.
        let messages = parsed["messages"].as_array().expect("messages stay an array");
        assert!(messages.len() < 40_000, "something had to go");
        // The marker is inserted at whichever array was shortened, so search the
        // whole document rather than guessing its depth.
        assert!(
            out.contains("bytes elided by tab-atelier-proxy"),
            "the reader is told what was dropped"
        );
        assert!(!messages.is_empty(), "both ends survive");
    }

    #[test]
    fn a_body_that_is_not_json_still_keeps_both_ends() {
        // The last resort: a body that cannot be parsed cannot be cut safely, so
        // both ends are kept and the notice says what went. It will not parse,
        // which is exactly why this is not the first choice.
        let big = "x".repeat(MAX_BODY + 1024);
        let (out, cut) = clamp(&big);
        assert!(cut);
        assert!(out.contains("bytes omitted by tab-atelier-proxy"));
        assert!(out.len() < big.len());
    }

    /// The date is stripped, so the label outlives the revision that carries
    /// it: `web_search_20250305` today, whatever Anthropic dates next.
    #[test]
    fn a_versioned_type_names_its_family() {
        let body = json!({"tools": [
            {"name": "Read", "description": "x"},
            {"type": "web_search_20250305", "name": "web_search"},
        ]});
        assert_eq!(server_tool(&body).as_deref(), Some("web_search"));
    }

    /// The discriminator is the date, not the name. A caller may ship its own
    /// tool called `web_search`; with no `type` it is not Anthropic's, and
    /// labelling it as a server tool would attribute spend to the wrong side.
    #[test]
    fn an_untyped_tool_named_like_a_server_tool_is_not_one() {
        let body = json!({"tools": [{"name": "web_search", "input_schema": {}}]});
        assert_eq!(server_tool(&body), None);
    }

    /// Only `_` + eight digits counts as a version. A bare type, or one dated
    /// some other way, is not a server tool this code should claim to know.
    #[test]
    fn an_unversioned_type_is_not_a_server_tool() {
        for ty in ["computer", "custom", "web_search_2025", "web_search_2025030x"] {
            let body = json!({"tools": [{"type": ty, "name": "x"}]});
            assert_eq!(server_tool(&body), None, "{ty}");
        }
    }

    /// No tools array, or an empty one, is the overwhelmingly common case and
    /// must cost nothing and say nothing.
    #[test]
    fn no_tools_means_no_server_tool() {
        assert_eq!(server_tool(&json!({})), None);
        assert_eq!(server_tool(&json!({"tools": []})), None);
    }
}
