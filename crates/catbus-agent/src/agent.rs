// SPDX-License-Identifier: MPL-2.0

//! The agent loop: send user message → call Messages API → execute
//! tool calls → loop until the model stops asking for tools.
//!
//! Messages calls go to the **relay**, not to Anthropic: `call_relay()`
//! posts to `<relay>/relay/anthropic/v1/messages` with this machine's
//! relay token. The proxy behind that URL holds the subscription login
//! and injects the identity headers the upstream expects, so this crate
//! carries no OAuth credential, no refresh cycle, and no account.
//!
//! Two things still live here because they describe *us* rather than the
//! login:
//!
//! * The first system block is the identity, and it is chosen rather than
//!   fixed: an operator file replaces it, and the built-in Claude Code line is
//!   only sent when the served model is Anthropic. See [`crate::identity`] for
//!   why it is conditional — this crate talks to relays that are not Anthropic.
//! * Our rendering instructions go in the last system block, and every block is
//!   static — the working directory and permission mode are a trailing turn, so
//!   that changing either does not invalidate the cached prefix.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use std::fmt::Write as _;

use crate::relay::Relay;
use crate::session::{self, Block, Session};
use crate::tools;
use crate::{ansi, cache, guard, progress, retry};

/// The model to ask for when neither the session nor the provider names one.
///
/// Sticking to a non-thinking, non-1M-context model keeps the bring-up surface
/// small. A session that has already chosen one keeps it — see
/// [`Session::saved_model`], which is where the choice really lives, and
/// `/model`, which is how it is made.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Output-token ceiling for a Messages call.
///
/// A named constant rather than a literal at the request site, because the
/// truncation warning below quotes it: writing the number in two places is how
/// a message comes to promise a limit the request does not use, which is exactly
/// what happened to the round-cap warning that claimed "32" while the default
/// was 200.
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// What the model is told when its previous reply stopped at the output limit.
///
/// Sent as a `user` message because that is the only role the API lets this side speak in, but
/// it is not from the operator and is never written to the transcript — see where it is pushed.
/// It says the reply was cut off, that the text so far is complete *as text*, and that what is
/// wanted is the remainder and nothing else: a model asked to continue will otherwise helpfully
/// restate the part that already arrived, and the operator would read it twice.
///
/// "Do not apologise" is in there for the same reason it is in most of these: an apology costs
/// a paragraph of the budget that is already too small.
const CONTINUATION_CUE: &str = "\
Your previous message was cut off at the output-token limit, not by you finishing. Continue \
from exactly where it stopped, mid-sentence if that is where the cut fell — repeat nothing you \
already wrote, do not restate the question, do not start over, and do not apologise. \
If a tool call was cut in half, make that call again from the start with the full arguments.";

/// What the model is asked to write, and why it changed.
///
/// Nothing session-specific may be added to this text: the working directory and
/// the permission mode travel in a trailing turn instead (see [`cache`]), because
/// putting mutable bytes in the middle of the prompt invalidates everything after
/// them on every toggle.
///
/// This used to ask for ANSI SGR escapes on a terminal sink and plain prose on
/// others. Both were wrong. Asking for escapes made the model's own formatting
/// survive only where something interpreted it — a live session answered into a
/// sink that rendered nothing and arrived as literal `[1;36m`, which is emphasis
/// turning into noise. And escapes are expensive: an SGR sequence is its own
/// token run on every emphasised word, in a request whose history is re-sent
/// every turn.
///
/// Markdown is the one instruction that works for every sink at once. The REPL
/// renders it (see `tui::markdown`), so a terminal gets bold headings and aligned
/// tables; anything that renders nothing — the socket, the phone, the transcript
/// a mirror reads — gets `**bold**`, which is legible and, unlike an escape,
/// survives a copy-paste. There is no second variant, because there is no longer
/// a case where the answer should be shaped differently.
///
/// The width note is not decoration: a table the terminal has to wrap is not a
/// table, and the renderer cannot unwrap one after the fact.
const INSTRUCTIONS_MARKDOWN: &str = "Your text replies are rendered in a terminal that renders \
    markdown. Write markdown: `#` or `##` for headings, `**bold**` for emphasis, \
    `- ` for bullets, `1. ` for numbered steps, and `` `code` `` for anything \
    literal. For tabular data use a pipe table — a header row, a `|---|---|` \
    rule line, then the rows — and keep it narrow enough to read in 80 columns, \
    because a table that has to wrap is not a table. Do NOT emit ANSI escape \
    sequences or colour codes: they are shown literally, they cannot be copied, \
    and they are stripped before your answer is displayed.";

/// Which API backend answers this session's prompts. Chosen once at
/// startup from the CLI; the tool loop is backend-agnostic.
pub enum Provider {
    /// Anthropic Messages API through a tab-atelier relay, which holds
    /// the subscription login.
    Relay(Relay),
    /// Any `OpenAI`-compatible chat-completions endpoint (`x.ai`/Grok,
    /// Infomaniak AI Tools, `OpenAI`, a local server, ...).
    OpenAiCompat(crate::openai::Config),
}

/// Turn a provider's error response into a sentence.
///
/// What this replaces, from a live session:
///
/// ```text
/// error: api: 503 Service Unavailable: {"error":{"message":"Service is too busy. We
/// advise users to temporarily switch to alternative LLM API service providers.",
/// "type":"service_unavailable_error"
/// ```
///
/// The useful sentence was in there, wrapped in braces and cut off mid-string by the
/// truncation limit, with nothing to suggest it was the part worth reading. The
/// envelope is the same on every provider this talks to — an `error` object with a
/// `message` and a `type` — so it is worth unwrapping rather than printing.
///
/// A body that does not match is printed as it came. A shape we do not recognise is
/// still information, and inventing an explanation for it would be worse than showing
/// it.
fn api_error(status: reqwest::StatusCode, body: &str) -> String {
    /// What a person needs from an unrecognisable body: the start of it.
    const BODY_LIMIT: usize = 300;

    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let field = |path: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.pointer(path))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    let mut out = status.to_string();
    if let Some(kind) = field("/error/type") {
        let _ = write!(out, " — {kind}");
    }
    // `error` holding a bare string is the other common shape, and a top-level
    // `message` is what a proxy that unwraps one level less produces.
    let message = field("/error/message")
        .or_else(|| field("/message"))
        .or_else(|| field("/error"));
    // `write!` rather than `push_str(&format!(..))`: it takes the value by reference
    // itself, so there is no intermediate `String` to borrow and no borrow for clippy
    // to object to.
    let _ = write!(
        out,
        ": {}",
        message.map_or_else(
            || truncate(body.trim(), BODY_LIMIT),
            |message| truncate(message, BODY_LIMIT),
        )
    );

    // What happened, not what to do about it. The provider's own sentence is printed
    // whole, advice and all — trimming an arbitrary message would be string surgery on
    // text whose shape we do not own — so adding advice here would contradict it
    // ("switch providers" against "try again"), and the operator can read the
    // provider's suggestion for themselves. What they cannot see is that this process
    // already waited and tried, which is the fact worth adding.
    if crate::retry::is_retryable(status.as_u16()) {
        out.push_str("\n  retried, and the provider was still failing");
    }
    out
}

/// What one turn produced.
///
/// `answer` and `reasoning` are separate channels rather than one string, because
/// they are shown differently and consumed by different clients: a harness reading
/// `done.text` wants the answer, and folding the model's deliberation into it would
/// change what every existing consumer displays. The transcript keeps both, in the
/// shape Claude Code writes (see `plain_content`), so a reader on disk sees the
/// same conversation either way.
pub struct Turn {
    /// What the operator asked for.
    pub answer: String,
    /// The model's own deliberation, when it produced any. Empty for a model that
    /// does not think.
    pub reasoning: String,
}

/// Session + history bundled together so swapping sessions mid-REPL
/// is atomic — we hold one write-lock and replace both at once.
struct ActiveSession {
    session: Arc<Session>,
    history: Vec<ApiMessage>,
}

pub struct Agent {
    provider: Provider,
    http: reqwest::Client,
    active: tokio::sync::RwLock<ActiveSession>,
    /// What the agent may do. Three states rather than the boolean this was —
    /// see [`tools::Gate`] for why the middle one is the point.
    gate: std::sync::atomic::AtomicU8,
    /// The model auto mode grades with.
    ///
    /// Separate from [`DEFAULT_MODEL`] on purpose: judging every write with
    /// the agent's own heavy model would cost an order of magnitude more than
    /// the judgement is worth, and the judge's prompt is fixed so it caches
    /// after the first call either way.
    judge_model: String,
    /// The text the judge is given as its system prompt. See [`guard`].
    monitor_prompt: String,
    /// The tools this agent offers, resolved once from the config.
    ///
    /// Held rather than recomputed, and that is a cache property: the tool
    /// array leads the body, so a set that varied between requests would
    /// invalidate every breakpoint behind it. See [`crate::tools::ToolSet`].
    tools: tools::ToolSet,
    /// Whether this session's output may be styled. Read through
    /// [`Self::styles_output`], which is the only place that decides.
    ansi: bool,
    /// This session's id, kept here so a status report can name it without awaiting a lock.
    ///
    /// `active` holds the session behind an `RwLock`, and the id changes on `swap_session`. Reporting
    /// status is fire-and-forget and must never delay a turn, so reading it from a dedicated mutex is
    /// what lets the report start in a spawned task with no await before it.
    session_id: std::sync::Mutex<String>,
    /// The channel a question from [`crate::tools::ask`] travels down.
    ///
    /// On the agent rather than inside the tool, because the answer comes from whatever is
    /// driving the session — the REPL or a socket client — and both need to reach it. Shared,
    /// so the tool can wait while the UI renders the question.
    asker: std::sync::Arc<crate::tools::ask::Asker>,
    /// What this session has used, and what the relay says it costs.
    ///
    /// Restored from the session's sidecar rather than started empty: resuming continues a
    /// session, and a session that forgot it had spent money the moment it was reopened would be
    /// worse than showing no cost at all. The amounts are stored for exactly that reason — they
    /// cannot be recomputed without the price list, and the price list may have changed.
    costs: std::sync::Arc<std::sync::Mutex<crate::cost::Costs>>,
    /// The model this session runs as.
    ///
    /// Session state, not a launch flag: `/model` sets it, the sidecar remembers
    /// it, and the transcript cannot supply it because the relay rewrites the
    /// model field on the way back. Seeded from the sidecar, then from the
    /// provider's own configured model, then from [`DEFAULT_MODEL`]. In a `Mutex`
    /// because `/model` changes it while a turn holds only `&self`.
    model: std::sync::Mutex<String>,
    /// What to send as the system prompt. See [`crate::identity`].
    identity: crate::identity::Identity,
    /// The model the relay reported serving, from the last reply.
    ///
    /// Only knowable from a reply, so the first turn of a session has none — see
    /// the identity block in [`Self::call_relay`]. In a `Mutex` because the turn
    /// that learns it holds only `&self`: the agent is shared, and a reply can
    /// arrive on any of its tasks. Kept as the raw name rather than a flag so the
    /// log can say which model caused a change.
    served_model: std::sync::Mutex<Option<String>>,
    /// Current activity description shown in the REPL spinner.
    /// `None` = idle, `Some(s)` = description of what's happening.
    pub status: std::sync::Mutex<Option<String>>,
    /// The reasoning from the model call being streamed right now.
    ///
    /// Written as the deltas arrive and read by the REPL on its redraw tick,
    /// which is why it is shared state and not a channel: the agent runs inside
    /// an `Arc` shared with the UI, there is no event loop to push into, and the
    /// one existing "show me what it is doing" channel — [`Self::status`] — works
    /// exactly this way.
    ///
    /// It is what to look at *while* the model works, not a record, and it is
    /// deliberately the current call rather than the whole turn: a turn runs one
    /// call per tool round, and what is worth watching is what the model is
    /// working out now. The turn's accumulated reasoning reaches the transcript
    /// through [`Turn::reasoning`] as it always did, and this is cleared as a
    /// turn starts so it is never a stale copy left on screen.
    reasoning: std::sync::Mutex<String>,
    /// Cumulative tokens consumed across all turns in this process.
    /// Both counters accumulate monotonically and are never reset.
    pub tokens_in: std::sync::atomic::AtomicU64,
    pub tokens_out: std::sync::atomic::AtomicU64,
    /// Serialised size of the request currently in flight, in bytes, or 0 when
    /// none is. Held as a byte count rather than a token estimate so the
    /// conversion lives in one place (`statusline::estimate_input_tokens`) and
    /// can change without touching this field.
    pub inflight_input_bytes: std::sync::atomic::AtomicU64,
    /// What the provider has reported about the request in flight, as it streams past.
    ///
    /// Reset as each request is sent and written per chunk by the assembler, so it describes the
    /// request being answered and never a previous one. Deliberately *not* cleared when the
    /// request finishes: a turn that is between tool rounds still has a cost worth seeing, and a
    /// figure that blinked out during every tool call would be harder to read than one that stays
    /// up until the next request replaces it.
    live: std::sync::Mutex<LiveReport>,
    /// Cancellation flag for the currently-running turn. Re-built at
    /// the start of every `run_user_prompt` so Ctrl+C only kills the
    /// in-flight request, not future ones.
    cancel: std::sync::Mutex<CancellationToken>,
}

/// What the provider has said so far about the request in flight.
///
/// Two sources with different reliability, kept apart rather than blended: the counts the provider
/// reported (authoritative, but the output half arrives only at the close) and the bytes of reply
/// that have arrived (a count of what came over the wire, which is what there is to show until
/// then). [`live_figures`] decides which of them a figure comes from.
#[derive(Debug, Clone, Default)]
struct LiveReport {
    /// Whether the reply has opened — see [`crate::stream::Assembler::started`]. From that moment
    /// the input count is the provider's own and never changes.
    started: bool,
    /// The counts the provider has reported: the input side at the head, the output side running.
    usage: Usage,
    /// Bytes of reply content received, which is the only output figure before the close.
    output_bytes: u64,
    /// The model the reply named, which is what the price on the row is computed from.
    model: Option<String>,
}

/// Reconcile the provider's report with the local measurement into what the row shows.
///
/// Split from the accessor because this is where every judgement lives — when a figure is an
/// estimate, when it is the provider's, and when there is nothing worth showing at all — and none
/// of it needs an agent, a session or a network to test.
///
/// The two sources are not interchangeable and the difference is visible on screen:
///
/// * **Before the reply opens**, the only number is the request's own byte length, and the row
///   says `~` because that is arithmetic rather than a count. The wording is the one the row has
///   always used for it.
/// * **Once it opens**, the input side is the provider's own count and loses the `~`; the output
///   side stays an estimate until the closing `message_delta`, which is the only frame carrying
///   the real one. Both are shown, because a generation that has produced 40 tokens of a reply
///   that will run to 4,000 is a fact about how far along it is.
/// * **`None`** when nothing has been measured and nothing reported, which is the row's state
///   between turns. A request whose payload has been serialised but not yet sent still counts as
///   something to show: the estimate is exactly what the row displayed before this existed.
fn live_figures(report: LiveReport, inflight_bytes: u64) -> Option<crate::statusline::Live> {
    if !report.started && report.output_bytes == 0 && inflight_bytes == 0 {
        // Nothing reported and nothing measured, so the row has nothing to say about cost. That is
        // the state between turns, and between the payload being serialised and being sent.
        return None;
    }
    let estimate = crate::statusline::estimate_input_tokens(usize::try_from(inflight_bytes).unwrap_or(usize::MAX));
    // The provider's input count supersedes the estimate the moment it exists; the output count
    // is the running one until `message_delta` closes the reply, and is estimated from the bytes
    // received until it does. A provider that reported an output count mid-stream (none does
    // today) would be taken at its word: the byte estimate is only ever a stand-in for a count.
    let output = if report.usage.output_tokens > 0 {
        report.usage.output_tokens
    } else {
        crate::statusline::estimate_input_tokens(usize::try_from(report.output_bytes).unwrap_or(usize::MAX))
    };
    let mut usage = if report.started {
        report.usage
    } else {
        // A reply that has produced content without opening — a stream that skipped
        // `message_start`, which providers do send — leaves the input side on the request's own
        // length. Better a figure marked as an estimate than none at all.
        Usage {
            input_tokens: estimate,
            ..Usage::default()
        }
    };
    usage.output_tokens = output;
    Some(crate::statusline::Live {
        usage,
        reported: report.started,
        output_estimated: report.usage.output_tokens == 0,
        model: report.model,
    })
}

/// What [`Agent::clear`] replaced, so the REPL can offer a way back.
///
/// A struct rather than a tuple so the two strings cannot be swapped by
/// accident at a call site: both are identifiers that look alike, and printing
/// the name where the id belongs would tell the operator to run
/// `/resume <name>`, which does not work.
pub struct Cleared {
    /// Pass to `/resume` to return to the replaced session.
    pub id: String,
    /// Human-readable label, for saying *which* conversation was left behind.
    pub name: String,
}

/// A turn's status bookkeeping, undone however the turn ends — including by being *dropped*.
///
/// The marker and the app's last word used to be written on the lines after the turn's `.await`, which
/// is the one place that is not safe: a turn is not always finished, it is sometimes *abandoned*,
/// whenever its future is dropped. Both cancellation paths do exactly that. Ctrl-C in the TUI aborts
/// the task, and a socket client that hangs up mid-turn makes `run_watching_for_questions` return
/// `Ok(None)` while the turn is still pinned inside it (`socket`). Neither runs a line after that
/// await, so the status line went on saying `thinking` and the tab's indicator stayed green — the app
/// reading "working" — until its own staleness sweep dropped the state 120 s later. An agent sitting
/// still is the one thing the indicator must never claim about it, which is what this guard is for.
///
/// Holds `self` rather than a copy of the session id: `/resume` can swap the session mid-turn, and the
/// end of a turn belongs to whichever session is current when it ends. No `unsafe`, so the guard is not
/// `Send`; that is fine, as it lives entirely inside the turn's own future.
struct TurnStatus<'a> {
    agent: &'a Agent,
}

impl Drop for TurnStatus<'_> {
    fn drop(&mut self) {
        *self.agent.status.lock().expect("status mutex") = None;
        // And the app hears that the turn is over: the operator is the one being waited on now, which
        // is what `waiting` means for a Claude Code tab too.
        self.agent.report_status(crate::applink::State::Waiting, None);
    }
}

impl Agent {
    #[must_use]
    pub fn new(provider: Provider, session: Session) -> Self {
        // A session that is opened rather than created already has a transcript:
        // `--resume <id>` names one, and the default "resume the newest session
        // in this cwd" path picks one. The history is rebuilt from it here, the
        // same way `swap_session` does for the in-REPL `/resume <id>`, so that
        // reopening a session continues the conversation the transcript
        // describes. Without this the agent appended to a transcript it had
        // never read: the model answered with no context, and every turn it
        // wrote was a non-sequitur on disk. A fresh session has an empty
        // transcript, so this costs it nothing.
        let history = rebuild_history(&session.project_dir, &session.id);
        // The mode this session was last left in, so it survives a restart. Tab
        // Atelier restarts the agent on a tab reopen, and a mode held only in
        // memory is lost exactly when the operator comes back to check it —
        // which is why `/auto` could look like it did nothing.
        let gate = session.saved_gate().unwrap_or(tools::Gate::Open);
        // Read now, before `session` is moved into the `Arc` below.
        let session_id = session.id.clone();
        // What this session had already spent, so a resume continues the count instead of
        // starting from zero. Both halves matter: the token counts are what the status lines
        // show, and the amounts cannot be recomputed here at all — the price list arrives later
        // and may differ, so the money is read back as it was recorded.
        let spent = session.load_tokens();
        let restored = crate::cost::Costs::restore(spent.as_ref());
        let seed = restored.tokens();
        let costs = restored;
        // The atomics the transcript and the sidecar read are seeded from the same file, so the
        // two views of the session agree from the first turn onwards.
        let tokens_in = std::sync::atomic::AtomicU64::new(seed.input);
        let tokens_out = std::sync::atomic::AtomicU64::new(seed.output);
        // The session's own model, so reopening continues with it. Read once and
        // used for both the request and the identity decision below.
        let from_transcript = session.last_model();
        let model = session
            .saved_model()
            // The provider's own config names a model explicitly on the
            // OpenAI-compatible path, so it is a better default there than ours.
            .or_else(|| match &provider {
                Provider::OpenAiCompat(config) => Some(config.model.clone()),
                Provider::Relay(_) => None,
            })
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        Self {
            provider,
            http: reqwest::Client::builder()
                .user_agent("catbus-agent/0.1 (tab-atelier)")
                .build()
                .expect("http client init"),
            active: tokio::sync::RwLock::new(ActiveSession {
                session: Arc::new(session),
                history,
            }),
            gate: std::sync::atomic::AtomicU8::new(gate.to_bits()),
            judge_model: crate::guard::DEFAULT_JUDGE_MODEL.to_owned(),
            monitor_prompt: crate::guard::MONITOR_PROMPT.to_owned(),
            tools: tools::ToolSet::builtin(),
            // Off unless the caller asks. A session is usually mirrored by
            // something with no terminal — the phone, an API client, the
            // transcript — and escape sequences are worse than useless there.
            // Opting in is a one-line change for the launcher, which is the
            // only party that knows what stdout actually is.
            ansi: false,
            // A fresh channel per agent; the REPL or a socket client reaches it through
            // `asker()`. See the field.
            session_id: std::sync::Mutex::new(session_id),
            asker: std::sync::Arc::new(crate::tools::ask::Asker::new()),
            // Seeded from the sidecar, so a resumed session carries on from what it had spent
            // rather than from zero. See `Session::load_tokens`.
            costs: std::sync::Arc::new(std::sync::Mutex::new(costs)),
            // Built-in behaviour until an operator says otherwise. See
            // `crate::identity`.
            identity: crate::identity::Identity::Auto,
            // Everything learned from the transcript is seeded here, so a resumed
            // session starts knowing rather than discovering — which is what makes
            // the first request of a resumed session behave like the last request
            // of the session it is continuing, instead of like a brand-new one.
            model: std::sync::Mutex::new(model),
            served_model: std::sync::Mutex::new(from_transcript),
            status: std::sync::Mutex::new(None),
            reasoning: std::sync::Mutex::new(String::new()),
            tokens_in,
            tokens_out,
            inflight_input_bytes: std::sync::atomic::AtomicU64::new(0),
            live: std::sync::Mutex::new(LiveReport::default()),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
        }
    }

    /// Trip the cancellation token for the in-flight turn (if any).
    /// Safe to call when nothing is running — the next `run_user_prompt`
    /// installs a fresh token before doing any work.
    pub fn cancel_current(&self) {
        self.cancel.lock().expect("cancel mutex").cancel();
    }

    /// Set the permission mode.
    ///
    /// Takes the whole [`tools::Gate`] rather than a bool so that the third
    /// state cannot be forgotten at a call site — the compiler asks which of
    /// the three every time, which is the point of replacing the boolean.
    ///
    /// Nothing else happens here, and in particular the transcript is not
    /// touched. The state turn is rendered into each request by
    /// [`Self::call_relay`], always last, so the model always sees the current
    /// mode and no historical turn ever holds a stale one.
    /// What the turn in flight is doing, if anything — a tool name, or a phase.
    ///
    /// Set by the tool loop and cleared when the turn ends. Read by a UI that wants
    /// to say more than "busy": the difference between waiting on a model and
    /// waiting on a `Bash` command is the difference between patience and alarm.
    #[must_use]
    pub fn status(&self) -> Option<String> {
        self.status.lock().expect("status mutex").clone()
    }

    /// The reasoning from the model call currently being streamed.
    ///
    /// Empty when nothing has been said yet, and when nothing is running. Read by
    /// the REPL on its redraw tick to put something on screen to watch; it is not
    /// a record of anything, and the turn's accumulated reasoning reaches the
    /// transcript through [`Turn::reasoning`] as before.
    #[must_use]
    pub fn reasoning_so_far(&self) -> String {
        self.reasoning.lock().expect("reasoning mutex").clone()
    }

    /// Forget the last turn's reasoning, at the start of a new one.
    ///
    /// Cleared here rather than when the turn ends so the screen keeps its last
    /// line until the answer actually arrives, instead of going blank for the
    /// time it takes the reply to be formatted and printed.
    fn clear_reasoning(&self) {
        self.reasoning.lock().expect("reasoning mutex").clear();
    }

    /// Forget everything the previous request reported about itself.
    ///
    /// The reasoning and the live cost are cleared together and deliberately, because they are the
    /// same fact about the same request: one is what it is saying, the other is what it is costing,
    /// and both describe the call in flight rather than the session. Clearing one and not the other
    /// is how the row would price a fresh request at the previous one's rates for the round trip —
    /// a number that is not stale enough to look broken and not fresh enough to be true.
    ///
    /// Called as each request is sent, not when a reply lands: the last line stays on screen until
    /// it is replaced, which is what keeps the row from blinking out between a reply and the tool
    /// call that follows it.
    fn clear_live(&self) {
        self.clear_reasoning();
        *self.live.lock().expect("live mutex") = LiveReport::default();
    }

    /// The question channel. See the field.
    #[must_use]
    pub const fn asker(&self) -> &std::sync::Arc<crate::tools::ask::Asker> {
        &self.asker
    }

    /// Tell tab-atelier what this session is doing, when tab-atelier is what is running it.
    ///
    /// Spawned rather than awaited: a status report is the least important thing in the process and
    /// must not add latency to a turn, so the task is left to finish on its own. [`crate::applink`]
    /// does nothing at all when there is no app, which is the ordinary case for a standalone install.
    fn report_status(&self, state: crate::applink::State, label: Option<String>) {
        let Some(endpoint) = crate::applink::endpoint() else {
            return;
        };
        let session = self.session_id.lock().map(|id| id.clone()).unwrap_or_default();
        if session.is_empty() {
            return;
        }
        // `try_current` where `spawn` would do, because this is now also reached from a `Drop` — and a
        // `Drop` can run on a thread that has no runtime at all: a turn future that is dropped rather
        // than polled never enters one, so abandoning a turn (see `TurnStatus`) is exactly the case
        // where there may be no runtime. `spawn` panics there, which would take the process down over
        // a status report. Losing the report is the ordinary case anyway, so it is logged and dropped.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            log::debug!("no runtime to report {state:?} from; the app keeps its last word");
            return;
        };
        handle.spawn(async move {
            crate::applink::report(&endpoint, state, label.as_deref(), &session).await;
        });
    }

    /// This session's id, for a caller that has to name it — the exit report in `main`, which runs
    /// after the REPL has returned and cannot await a lock.
    #[must_use]
    pub fn session_id_for_report(&self) -> String {
        self.session_id.lock().map(|id| id.clone()).unwrap_or_default()
    }

    /// The running totals. See [`crate::cost`].
    #[must_use]
    pub fn costs(&self) -> std::sync::Arc<std::sync::Mutex<crate::cost::Costs>> {
        std::sync::Arc::clone(&self.costs)
    }

    /// Fetch what the relay charges, once, and remember it.
    ///
    /// Best-effort by design: a relay that serves no price list, or none this can read, leaves
    /// the session counting tokens with no amounts rather than failing. Losing a price must not
    /// cost a session, and an unknown price is not a price of zero — so a caller logs the
    /// failure and the totals report `unpriced` instead of inventing a figure.
    pub async fn fetch_prices(&self) -> Result<(), String> {
        let Provider::Relay(relay) = &self.provider else {
            // A hand-configured OpenAI-compatible endpoint has no relay to ask and no convention
            // for what it charges. Counting tokens is all that is honest there.
            return Err("prices come from a relay; this session runs against a direct endpoint".to_owned());
        };
        let url = relay.models_url();
        let response = Self::with_relay_auth(relay, self.http.get(&url))
            .header("accept", "application/json")
            // Closing rather than pooling, even though this client pools everything else. The
            // price list is fetched once, so a kept-alive connection buys nothing — and it costs
            // something: a server that answers and then closes leaves a socket in the pool that
            // the *next* request may pick up, and a POST is not retried, so the failure surfaces
            // as the first real turn failing to send. Asking for a close makes the client discard
            // the connection instead of trusting it.
            .header("connection", "close")
            .send()
            .await
            .map_err(|e| format!("could not reach {url}: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{url} did not serve a price list ({status}). A relay that has none is fine — \
                 tokens are still counted."
            ));
        }
        let catalog = crate::cost::Catalog::parse(&body)?;
        if catalog.is_empty() {
            return Err("the price list named no usable model".to_owned());
        }
        if let Ok(mut costs) = self.costs.lock() {
            costs.set_catalog(catalog);
        }
        Ok(())
    }

    /// The headers every relay request carries.
    ///
    /// One construction for every call, because the auth is the last thing that should differ
    /// between a request that works and one that does not: the models fetch and the messages
    /// call must present the same credentials, or one of them fails with no visible reason.
    fn with_relay_auth(relay: &Relay, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request
            .header("x-api-key", relay.token())
            .header("anthropic-version", claude_api::ANTHROPIC_VERSION);
        if let Some((client_id, client_secret)) = relay.cloudflare_access() {
            request = request
                .header("CF-Access-Client-Id", client_id)
                .header("CF-Access-Client-Secret", client_secret);
        }
        request
    }

    /// Whether this session's output may be styled.
    ///
    /// `--ansi` / `NO_COLOR` used to decide the prompt's wording and whether a reply
    /// was filtered. Neither is true now: the prompt is markdown for every sink, and
    /// escapes are always stripped. What is left, and what this flag should always
    /// have meant, is whether the *renderer* styles what it shows — a terminal that
    /// renders markdown gets headings and tables, and `NO_COLOR` gets the same text
    /// with the syntax visible and nothing coloured.
    #[must_use]
    pub const fn styles_output(&self) -> bool {
        self.ansi
    }

    /// The model this session runs as. See the field for why it is not a flag.
    #[must_use]
    pub fn model(&self) -> String {
        self.model.lock().expect("model mutex").clone()
    }

    /// Choose the model for the rest of this session, and remember it.
    ///
    /// Remembered rather than merely set, because the model is a property of the
    /// session and not of this process: Tab Atelier restarts the agent whenever a
    /// tab is reopened, so a choice held in memory is lost exactly when the
    /// operator comes back — which is why `/model` would otherwise look like it did
    /// nothing. See [`Session::saved_model`].
    ///
    /// A blank name is refused. A failure to record is logged rather than returned:
    /// the in-memory value is what this process uses.
    pub async fn set_model(&self, model: &str) -> Result<(), String> {
        let name = model.trim();
        if name.is_empty() {
            return Err("a model name is required".to_string());
        }
        name.clone_into(&mut self.model.lock().expect("model mutex"));
        let session = self.active.read().await.session.clone();
        if let Err(e) = session.save_model(name) {
            log::warn!("could not record model `{name}` for this session: {e}");
        }
        Ok(())
    }

    /// Set the permission mode, and remember it for the next time this session is
    /// opened. See [`Session::saved_gate`] for why the memory outlives the
    /// process.
    pub async fn set_gate(&self, gate: tools::Gate) {
        self.gate.store(gate.to_bits(), std::sync::atomic::Ordering::Relaxed);
        // A failure here must not lose the mode the operator just chose: the
        // in-memory value is what this process uses, and the sidecar only decides
        // what a *later* process starts with. So it is logged rather than
        // returned — the alternative is a mode that silently does not stick,
        // which is the bug this is fixing.
        let session = self.active.read().await.session.clone();
        if let Err(e) = session.save_gate(gate) {
            log::warn!("could not record gate `{}` for this session: {e}", gate.as_str());
        }
    }

    /// The current permission mode.
    #[must_use]
    pub fn gate(&self) -> tools::Gate {
        tools::Gate::from_bits(self.gate.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Input tokens billed this process, cumulative across every prompt.
    ///
    /// Accessors rather than reading the public atomics directly, so a caller
    /// does not have to name an `Ordering` to display a number — `Relaxed` is
    /// correct here (the value is only ever shown, never used to order other
    /// accesses), and it is the kind of detail a call site should not repeat.
    #[must_use]
    pub fn total_tokens_in(&self) -> u64 {
        self.tokens_in.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Output tokens billed this process, cumulative across every prompt.
    #[must_use]
    pub fn total_tokens_out(&self) -> u64 {
        self.tokens_out.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What the request in flight has cost so far, or `None` when there is nothing to show.
    ///
    /// This is the figure the status row puts a *price* on while the model works, which is the
    /// point of it: the totals line under a finished answer is the same arithmetic applied to a
    /// turn that is already paid for, and by then the decision an operator wanted the number for
    /// has been made. See [`statusline::Live`] for what each count means and
    /// [`live_figures`] for how the two sources are reconciled.
    #[must_use]
    pub fn live_cost(&self) -> Option<crate::statusline::Live> {
        let report = self.live.lock().expect("live mutex").clone();
        let bytes = self.inflight_input_bytes.load(std::sync::atomic::Ordering::Relaxed);
        live_figures(report, bytes)
    }

    /// The prices the catalog holds for a model, if it holds any.
    ///
    /// A pass-through rather than a second place that knows how to find a price: the status row
    /// reaches the catalog through the agent because the agent owns it, not because the lookup is
    /// the agent's business.
    #[must_use]
    pub fn price_of(&self, model: &str) -> Option<crate::cost::ModelPrice> {
        self.costs.lock().ok().and_then(|c| c.price_of(model))
    }

    /// Configure the judge that auto mode consults.
    ///
    /// A builder rather than two more `new` parameters: the judge is only
    /// meaningful in auto mode, and every existing caller of `new` — the tests
    /// included — should keep working without naming a model it will never use.
    #[must_use]
    pub fn with_judge(mut self, model: String, prompt: String) -> Self {
        self.judge_model = model;
        self.monitor_prompt = prompt;
        self
    }

    /// Offer this tool set instead of the built-ins.
    #[must_use]
    pub fn with_tools(mut self, tools: tools::ToolSet) -> Self {
        self.tools = tools;
        self
    }

    /// Tell the agent whether its answers are read in a terminal.
    ///
    /// A builder rather than a `new` parameter because it describes the sink,
    /// not the agent, and most callers do not have a terminal to describe. The
    /// default is `false`: a session that is *mirrored* — read over the socket,
    /// stored in the transcript, shown on a phone — is the common case, and
    /// escape sequences there arrive as literal `[1m`. Only a caller that will
    /// actually render stdout (the REPL, when stdout is a tty) should opt in.
    #[must_use]
    pub const fn with_ansi(mut self, ansi: bool) -> Self {
        self.ansi = ansi;
        self
    }

    /// Use an operator-supplied system prompt. See [`crate::identity`].
    #[must_use]
    pub fn with_identity(mut self, identity: crate::identity::Identity) -> Self {
        self.identity = identity;
        self
    }

    /// Grade one proposed action, in auto mode.
    ///
    /// Reads the transcript under a shared lock and renders it for the judge.
    /// A failure to render is not a failure to judge — an empty transcript is
    /// still something to grade, and the judge's own fail-closed rule covers
    /// the rest.
    async fn judge_action(&self, name: &str, input: &serde_json::Value, history: &[ApiMessage]) -> guard::Verdict {
        let Some((http_client, relay)) = self.relay_config() else {
            // Auto mode on a non-relay provider. There is no judge to consult,
            // and inventing one would mean a second endpoint this agent has no
            // credential for, so the action is refused rather than waved
            // through. Refusing is recoverable; allowing is not.
            return guard::Verdict::Unusable(
                "auto mode needs a relay to grade actions against, and this session is not using one".into(),
            );
        };
        let mut transcript = guard::Judge::render_transcript(history);
        // The action itself, appended as the last thing the judge sees. The
        // transcript ends at the assistant's proposal; naming the exact call
        // and its arguments is what makes the difference between grading an
        // intention and grading a command.
        //
        // `write!` rather than `push_str(&format!(…))`: the latter allocates a
        // whole temporary string to copy into one that already has room.
        let _ = write!(transcript, "\n=== proposed action ===\n{name} {input}\n");

        let judge = guard::Judge::new(http_client, relay, &self.judge_model, &self.monitor_prompt);
        judge.grade(&transcript).await
    }

    /// The relay and HTTP client, when this session is on the relay provider.
    #[must_use]
    const fn relay_config(&self) -> Option<(&reqwest::Client, &Relay)> {
        match &self.provider {
            Provider::Relay(relay) => Some((&self.http, relay)),
            Provider::OpenAiCompat(_) => None,
        }
    }

    /// Current session id — cheap to read, no lock held.
    pub async fn session_id(&self) -> String {
        self.active.read().await.session.id.clone()
    }

    /// Current session cwd.
    #[allow(dead_code)]
    pub async fn session_cwd(&self) -> std::path::PathBuf {
        self.active.read().await.session.cwd.clone()
    }

    /// Transcript path for the current session — used to print the
    /// resume preview.
    pub async fn transcript_path(&self) -> std::path::PathBuf {
        self.active.read().await.session.transcript_path()
    }

    /// Arc to the active session — used to save token sidecars after each turn.
    pub async fn active_session(&self) -> Arc<Session> {
        Arc::clone(&self.active.read().await.session)
    }

    /// Current session name (empty = unnamed).
    pub async fn session_name(&self) -> String {
        self.active.read().await.session.session_name()
    }

    /// Rename the current session.
    pub async fn rename_session(&self, name: &str) -> Result<(), crate::session::SessionError> {
        self.active.read().await.session.rename(name)
    }

    /// Swap in a different session. Rebuilds the in-memory history
    /// from the new transcript so the model has full context.
    pub async fn swap_session(&self, new_session: Session) -> Result<(), AgentError> {
        let history = rebuild_history(&new_session.project_dir, &new_session.id);
        // The id is updated too, and before the swap is visible: a different session is a different
        // transcript, and the app would otherwise keep the old id and resume the wrong conversation
        // when the tab is reopened. Set first so a report cannot land under a stale id.
        if let Ok(mut id) = self.session_id.lock() {
            new_session.id.clone_into(&mut id);
        }
        *self.active.write().await = ActiveSession {
            session: Arc::new(new_session),
            history,
        };
        // And said once, so the app has the new id immediately rather than at the next turn.
        self.report_status(crate::applink::State::Waiting, None);
        Ok(())
    }

    /// Forget the conversation and start a fresh session in the same working
    /// directory, replacing the active one.
    ///
    /// A *new* session, not an emptied history. That choice is deliberate:
    ///
    /// * the old transcript stays on disk untouched, so `/clear` cannot destroy
    ///   work — it is recoverable with `/resume` or by reading the file, which
    ///   is what makes it safe to offer at all;
    /// * the model gets exactly the context a brand-new session gets, with no
    ///   residue of a summary or a truncated tail;
    /// * the token sidecar under the old session id keeps describing a
    ///   conversation that still exists on disk.
    ///
    /// The name is not carried over — a name labels the transcript it belongs
    /// to, and this is a different transcript. The caller can `/rename` it.
    ///
    /// Deliberately does **not** reset `tokens_in`/`tokens_out`. Those are
    /// documented as process-cumulative and the REPL prints them as the cost of
    /// this run, so zeroing them here would under-report spend. `/clear` resets
    /// the conversation, not the meter.
    ///
    /// Returns the id and name of the session it replaced, so the caller can
    /// tell the operator which transcript to `/resume` if the clear was a
    /// mistake. Only these two are handed back: `Session` owns a `Mutex` and so
    /// is not `Clone`, and the caller needs nothing else.
    pub async fn clear(&self) -> Result<Cleared, AgentError> {
        let cwd = self.active.read().await.session.cwd.clone();
        let new_session = crate::session::open(&cwd, None, true)?;
        let history = rebuild_history(&new_session.project_dir, &new_session.id);
        let previous = {
            let mut active = self.active.write().await;
            let previous = Arc::clone(&active.session);
            *active = ActiveSession {
                session: Arc::new(new_session),
                history,
            };
            previous
        };
        Ok(Cleared {
            id: previous.id.clone(),
            name: previous.session_name(),
        })
    }

    /// One full turn: append the user's text to the transcript, then
    /// drive the tool loop until the assistant stops asking for
    /// tools. Returns the model's final assistant text concatenated.
    pub async fn run_user_prompt(&self, text: String) -> Result<Turn, AgentError> {
        // Fresh token per turn so a stale cancel doesn't kill the next
        // request before it even starts. Hold the lock only long enough
        // to swap; the inner future borrows the new clone.
        let token = {
            let mut slot = self.cancel.lock().expect("cancel mutex");
            *slot = CancellationToken::new();
            slot.clone()
        };
        *self.status.lock().expect("status mutex") = Some(crate::statusline::THINKING_MARKER.to_owned());
        // What the tab's indicator shows for the whole of a turn.
        self.report_status(crate::applink::State::Thinking, None);
        // Taken before the turn runs and held across it, so that taking it back cannot be skipped by a
        // turn that never returns — a Ctrl-C or a socket client hanging up drops this future instead of
        // finishing it, and the lines that used to undo this would never be reached. See `TurnStatus`.
        let turn_status = TurnStatus { agent: self };
        let result = self.run_user_prompt_inner(text, &token).await;
        // The turn is over, so the marker comes off and the app hears about it. Deliberately explicit
        // rather than left to the end of the function, so the order against `save_totals` below is the
        // one the reader sees.
        drop(turn_status);
        // The totals are persisted here rather than by whichever UI happens to be attached, so
        // every path that runs a turn records them: the REPL, a socket client, and the app's own
        // CLI. It was in the REPL's render path before, which meant a session driven over the
        // socket never wrote a sidecar at all — and a resume then had nothing to restore, so the
        // money and counts started again from zero.
        self.save_totals().await;
        result
    }

    /// Write the running totals beside the transcript, and say nothing if it fails.
    ///
    /// Best-effort: the file is for a later resume and for tab-atelier to read, and neither is
    /// worth failing a completed turn over — the answer is already in hand.
    async fn save_totals(&self) {
        let session = self.active_session().await;
        let cost = crate::cost::totals_of(&self.costs);
        if let Err(e) = session.save_tokens(self.total_tokens_in(), self.total_tokens_out(), &cost) {
            log::warn!("could not record the token totals: {e}");
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_user_prompt_inner(&self, text: String, cancel: &CancellationToken) -> Result<Turn, AgentError> {
        // Snapshot session Arc so we're not holding the RwLock across
        // await points in the tool loop.
        let session = Arc::clone(&self.active.read().await.session);

        // Persist + remember the user turn.
        let entry = session::user_text(&session, text.clone());
        session.append(&entry)?;
        {
            let mut active = self.active.write().await;
            active.history.push(ApiMessage {
                role: "user".into(),
                content: ApiContent::Plain(text),
            });
        }

        let mut final_text = String::new();
        // Separate from `final_text` so the answer keeps exactly the shape it had
        // before reasoning was carried: see `Turn`.
        let mut reasoning = String::new();
        // The live view starts each turn empty, so what is on screen is only
        // ever about the turn actually running.
        self.clear_live();
        // Cap on tool rounds. 200 is intentionally high — the model
        // self-terminates via end_turn long before this in normal use.
        // The env-var escape hatch exists for unusually long tasks.
        let max_rounds: u32 = std::env::var("CATBUS_MAX_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        // Survives across rounds on purpose: the two ways a loop shows itself —
        // the same call again, or the same result again — are only visible as a
        // run. See `progress` for the case that made this necessary.
        let mut loop_guard = progress::Progress::from_env();
        // Set when a round stopped at the output limit, so the next round is understood as the
        // second half of the same sentence rather than a new paragraph — see the text arm below
        // and `CONTINUATION_CUE`.
        let mut mid_sentence = false;
        // How many times one answer may be continued before the limit is reported instead.
        // Bounded because the limit is a property of the request, not of this answer: a model
        // that reasons its way to the ceiling will do it again next round, and an unbounded
        // loop would spend the whole turn doing that. Eight is several ordinary replies' worth
        // of text past the first stop, which is far more than a cut-off sentence needs.
        let max_continuations: u32 = std::env::var("CATBUS_MAX_CONTINUATIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let mut continuations: u32 = 0;
        for _ in 0..max_rounds {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            *self.status.lock().expect("status mutex") = Some(crate::statusline::THINKING_MARKER.to_owned());
            let resp = tokio::select! {
                res = self.call_messages() => res?,
                () = cancel.cancelled() => return Err(AgentError::Cancelled),
            };
            // Accumulate token usage immediately so the sidecar file
            // written after each prompt is always up to date.
            self.tokens_in
                .fetch_add(resp.usage.input_tokens, std::sync::atomic::Ordering::Relaxed);
            self.tokens_out
                .fetch_add(resp.usage.output_tokens, std::sync::atomic::Ordering::Relaxed);
            // And the priced view of the same turn. Recorded with the model the relay just
            // reported, so a session that spans providers prices each turn at the rate of the
            // model that actually served it — which is why the totals are grouped by currency
            // rather than summed into one figure.
            if let Ok(mut costs) = self.costs.lock() {
                costs.record(&resp.model, resp.usage.tokens());
            }
            // Persist a plain copy, not `resp.content` itself. The transcript is
            // a *shared* artifact read by clients with no terminal —
            // tab-atelier renders chat bubbles straight from it — so it stays
            // plain even for a session whose REPL is painting a terminal. That
            // is the reported bug: the reply reached the transcript exactly as
            // the model sent it, bypassing every socket-side filter.
            //
            // The visible answer is filtered separately, by `for_sink`, because
            // a terminal *should* keep its colour. So the two can legitimately
            // differ, and the in-memory history keeps the model's own bytes:
            // echoing them back verbatim is one less thing to get wrong, and
            // that is the copy the upstream sees.
            //
            // Still one clone: persist first, then iterate the blocks by
            // borrow, then move them into in-memory history without a second
            // copy.
            let entry = session::assistant_blocks(&session, resp.model.clone(), plain_content(&resp.content));
            session.append(&entry)?;

            // Remember what actually answered, and say so the first time it
            // changes shape. This is the only source for it: no reply yet means no
            // name, which is why the first turn of a session keeps the built-in
            // identity — see the system block in `call_relay`.
            {
                let mut served = self.served_model.lock().expect("served model mutex");
                let was_anthropic = served.as_deref().is_none_or(crate::identity::is_anthropic);
                if served.as_deref() != Some(resp.model.as_str()) {
                    let now_anthropic = crate::identity::is_anthropic(&resp.model);
                    if now_anthropic != was_anthropic {
                        // Once per change, not once per request: a model that is
                        // not Anthropic is the case an operator wants to know
                        // about, since it is the one where the Claude identity
                        // line stops going out.
                        log::info!(
                            "the relay is serving `{}`, so the Claude identity line will {} be sent",
                            resp.model,
                            if now_anthropic { "" } else { "no longer" }
                        );
                    }
                    *served = Some(resp.model.clone());
                }
            }

            // Collect tool_use blocks by reference; pull any text into
            // the visible answer so the caller has *something* even
            // mid-tool-use. The borrows into `resp.content` survive the
            // tool dispatch awaits below — the history push that moves
            // `resp.content` happens *after* this borrow goes out of scope.
            let mut tool_uses: Vec<(&str, &str, &serde_json::Value)> = Vec::new();
            for block in &resp.content {
                match block {
                    Block::Text { text } => {
                        // A newline separates turns of a tool loop, which are separate
                        // statements. It does not separate the two halves of one sentence, and
                        // a continuation is exactly that — see `mid_sentence` — so that case
                        // joins with nothing, which is what the model was told to do.
                        if !final_text.is_empty() && !mid_sentence {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                    }
                    Block::ToolUse { id, name, input } => {
                        tool_uses.push((id.as_str(), name.as_str(), input));
                    }
                    // A tool result is ours, not the model's. Reasoning is the
                    // model's own, and is carried beside the answer rather than
                    // inside it — a caller that only wants what it asked for reads
                    // `Turn::answer`, which is unchanged.
                    Block::Thinking { thinking, .. } => {
                        if !reasoning.is_empty() {
                            reasoning.push('\n');
                        }
                        reasoning.push_str(thinking);
                    }
                    Block::ToolResult { .. } => {}
                }
            }

            let stop_reason = resp.stop_reason.clone();
            // The test is whether there is tool work, not what `stop_reason`
            // says. A reply that carries `tool_use` blocks has to be answered
            // whatever the stop reason claims, because the API rejects a history
            // in which a `tool_use` has no matching `tool_result` — so an
            // `end_turn` that came with tool calls would end the turn here, push
            // the unanswered calls into history, and 400 every request after it
            // until the session was thrown away. The stop reason is still read
            // below for the `max_tokens` fragment warning.
            if tool_uses.is_empty() {
                // No tool work to do — end the borrow into resp.content and
                // move it straight into history. Inner scope keeps the
                // write-lock guard tight (clippy::significant_drop_tightening).
                let _ = tool_uses;
                {
                    let mut active = self.active.write().await;
                    active.history.push(ApiMessage {
                        role: "assistant".into(),
                        content: ApiContent::Blocks(resp.content),
                    });
                }
                // A reply cut off by the output limit is not a finished answer, yet this
                // branch is where the turn would end: the model stopped mid-sentence, so
                // there is no tool to continue with and no error to raise.
                //
                // Warning about it is not enough, and a warning is what this used to do:
                // the operator reads "ask for the rest", asks, and gets a *second* answer
                // that has to pick up a sentence it can no longer see the start of — when
                // the request is re-sent, the cut-off reply is in history, but what arrived
                // after it was the operator's words rather than the model's own train of
                // thought. The limit is a property of one request, so the honest fix is to
                // make the next request and let the model finish the thought.
                if stop_reason.as_deref() == Some("max_tokens") {
                    if continuations < max_continuations {
                        continuations += 1;
                        // The next round joins this text without a paragraph break, because
                        // the model is being asked to finish the sentence, not to start one.
                        mid_sentence = true;
                        // In memory only. The transcript keeps the model's words and nothing
                        // else, so a cue from us must not be written there: it would read, on
                        // resume, as a thing the operator had said. History is what the next
                        // request is built from, and that is all this needs to reach the model.
                        // Scoped so the guard is released before the log and the next round.
                        {
                            let mut active = self.active.write().await;
                            active.history.push(ApiMessage {
                                role: "user".into(),
                                content: ApiContent::Plain(CONTINUATION_CUE.to_owned()),
                            });
                        }
                        log::info!(
                            "response hit the {MAX_OUTPUT_TOKENS}-token output limit; asking the \
                             model to continue ({continuations}/{max_continuations})"
                        );
                        continue;
                    }
                    // Out of continuations, so the limit is real and worth reporting — but
                    // honestly: by now the model was asked to continue and stopped again, so
                    // the operator is told what was tried rather than told to try it.
                    let _ = write!(
                        final_text,
                        "\n\n\x1b[33m[reply still cut off at the {MAX_OUTPUT_TOKENS}-token output \
                         limit after {max_continuations} automatic continuations — it may end \
                         mid-sentence]\x1b[0m"
                    );
                }
                return Ok(Turn {
                    answer: for_sink(final_text),
                    reasoning: for_sink(reasoning),
                });
            }

            // Run tools, build a single user-message of tool_result
            // blocks (Messages API wants them all in one message,
            // in the same order the model produced the tool_use
            // blocks).
            let gate = self.gate();
            // Asked before anything is dispatched, because that is the whole
            // point of catching this shape: the call has nothing new to tell the
            // model, so making it costs the round and changes nothing. It is
            // refused — never dispatched — and answered with a `tool_result` that
            // says so; see `progress` for why the previous rounds' output has to
            // have stalled too before a repeat counts as a loop.
            let stop = loop_guard.before(tool_uses.iter().map(|(_, name, input)| (*name, *input)));
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in &tool_uses {
                if cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                // A refused call still gets a `tool_result`: the API requires one
                // for every `tool_use`, and the model reads the refusal as the
                // outcome of the call it made.
                if let Some(stop) = &stop {
                    results.push(Block::ToolResult {
                        tool_use_id: (*id).to_string(),
                        content: stop.refusal(),
                        is_error: true,
                    });
                    continue;
                }
                // Show the tool name (and a short input summary for Bash)
                // in the status so the spinner reflects what's running.
                let label = tool_status_label(name, input);
                *self.status.lock().expect("status mutex") = Some(label);

                // Auto mode grades a write-capable action before running it.
                //
                // Only write-capable ones: reading is never blocked, on the
                // monitor prompt's own exception, and judging a read would make
                // auto mode slower than plan-mode while guarding nothing.
                // `Delegate` is not judged here either — the child runs under
                // this same gate and judges its own actions, so grading the
                // spawn would charge twice for one decision.
                //
                // The verdict is turned into a `tool_result` rather than ending
                // the turn: the model needs to see the refusal and propose
                // something else, which is exactly the behaviour plan-mode
                // produces and the reason it is useful.
                // Every verdict is recorded, allowed or refused. A judge that
                // allows and says nothing is indistinguishable from one that
                // never ran, so a working gate can be reported as doing nothing —
                // and there is no way to tell a permissive judge from an absent
                // one. The record goes to the log and, when allowed, into the
                // tool result, which is where the operator or a later session
                // will actually look.
                let vetted: Option<String> = if gate.judges() && self.tools.call_changes_the_world(name, input) {
                    *self.status.lock().expect("status mutex") = Some(format!("checking {name}"));
                    let history = { self.active.read().await.history.clone() };
                    let verdict = self.judge_action(name, input, &history).await;
                    let record = format!("auto checked {name}: {}", verdict.summary());
                    log::info!("gate: {record}");
                    if verdict.blocks() {
                        results.push(Block::ToolResult {
                            tool_use_id: (*id).to_string(),
                            content: verdict.describe(),
                            is_error: true,
                        });
                        continue;
                    }
                    Some(record)
                } else {
                    None
                };

                // Reported for **every** tool call, not only the judged ones. It was inside the branch
                // above, so a session running in `open` mode — the default — never sent a label and the
                // tab's indicator stayed on `thinking` with nothing naming the tool. The label is the
                // same name the status row shows, so the tab and the agent cannot disagree about what
                // is running.
                self.report_status(crate::applink::State::Thinking, Some((*name).to_owned()));

                let (mut content, is_error) = tokio::select! {
                    out = self.tools.dispatch(name, input, &session.cwd, gate, &self.asker) => {
                        out.map_or_else(|e| (format!("Error: {e}"), true), |out| (out, false))
                    }
                    () = cancel.cancelled() => return Err(AgentError::Cancelled),
                };
                if let Some(record) = vetted {
                    content = format!("{content}\n\n[{record}]");
                }
                results.push(Block::ToolResult {
                    tool_use_id: (*id).to_string(),
                    content,
                    is_error,
                });
            }
            // The other half of the guard, on the signal this side can actually
            // see: if this round's results matched the round before, the model
            // learned nothing, whatever its calls looked like. This is what
            // catches a model varying its reads and getting the same answer —
            // the shape the 2026-09-25 loop took. Skipped when the calls were
            // already refused above: those results are the refusal text, and
            // feeding it to the result check would count a refusal as staleness.
            let stop = stop.or_else(|| {
                loop_guard.after(results.iter().filter_map(|block| match block {
                    Block::ToolResult { content, .. } => Some(content.as_str()),
                    _ => None,
                }))
            });
            // Done with the borrows — move resp.content into history now.
            let _ = tool_uses;
            // Whatever the next round says follows tool results, so it is a new statement rather
            // than the second half of the previous one: the no-separator rule in the text arm
            // applies to a continuation and to nothing else, and a continuation is over as soon
            // as the model has been given something to react to.
            mid_sentence = false;
            {
                let mut active = self.active.write().await;
                active.history.push(ApiMessage {
                    role: "assistant".into(),
                    content: ApiContent::Blocks(resp.content),
                });
            }
            let entry = session::tool_results(&session, results.clone());
            session.append(&entry)?;
            {
                let mut active = self.active.write().await;
                active.history.push(ApiMessage {
                    role: "user".into(),
                    content: ApiContent::Blocks(results),
                });
            }
            // Cut here, not at the round cap, and only after the round is
            // recorded: the history above is what keeps the turn valid — an
            // assistant message whose `tool_use` blocks have no answering
            // `tool_result` is a 400 on the next request, so the results go in
            // even when the loop stops. `final_text` carries whatever the model
            // said on the way, and the notice explains the stop to the reader.
            if let Some(stop) = stop {
                final_text.push_str(&stop.notice());
                return Ok(Turn {
                    answer: for_sink(final_text),
                    reasoning: for_sink(reasoning),
                });
            }
        }
        // Rounds exhausted. Return whatever text was collected so far so the
        // REPL shows it, and append a warning so the reader knows the loop was
        // cut short rather than silently losing output. The limit is
        // interpolated rather than written out: this message said "32-round cap"
        // while the default was 200, so an operator who believed it would raise
        // `CATBUS_MAX_ROUNDS` to a value *below* the real default and see no
        // change.
        if final_text.is_empty() {
            Err(AgentError::TooManyRounds { max_rounds })
        } else {
            let _ = write!(
                final_text,
                "\n\n\x1b[33m[tool loop hit the {max_rounds}-round cap — response may be incomplete; \
                 raise CATBUS_MAX_ROUNDS to allow more]\x1b[0m"
            );
            Ok(Turn {
                answer: for_sink(final_text),
                reasoning: for_sink(reasoning),
            })
        }
    }

    async fn call_messages(&self) -> Result<MessagesResp, AgentError> {
        match &self.provider {
            Provider::Relay(relay) => self.call_relay(relay).await,
            Provider::OpenAiCompat(cfg) => self.call_openai_compat(cfg).await,
        }
    }

    /// One Messages call, routed through the relay.
    ///
    /// We send the relay token as `x-api-key` — the header the proxy's
    /// client guard reads first — and deliberately send no `Authorization`,
    /// no `anthropic-beta` and no `x-app`: the relay owns those, resolving
    /// the credential and the beta flags for the upstream it picks. Sending
    /// ours as well would either be dropped or fight the relay's choice.
    async fn call_relay(&self, relay: &Relay) -> Result<MessagesResp, AgentError> {
        let active = self.active.read().await;
        let tool_specs = self.tools.specs().to_vec();
        // Both system blocks are static text, and the live state is the last
        // entry in `history` rather than a block here. It used to be the
        // *middle* block — before the static instructions — which meant a cwd
        // or gate change invalidated the block behind it and every message
        // after it. See `crate::cache`.
        // Which identity block to send. The operator's file, when there is one,
        // replaces the whole system prompt rather than being added to it —
        // whoever writes it owns what the model is told about itself. Failing
        // that, the Claude line goes out only when the model is Anthropic, or
        // when no reply has named a model yet: the first turn of a session cannot
        // know, so it keeps the old behaviour, and from the second on the served
        // model decides.
        let identity = self.identity.clone();
        // Held across the body construction: `MessagesReq` borrows it, so the
        // guard has to outlive the request value.
        let session_model = self.model.lock().expect("model mutex").clone();
        let non_anthropic = self
            .served_model
            .lock()
            .expect("served model mutex")
            .as_deref()
            .is_some_and(|model| !crate::identity::is_anthropic(model));
        let mut system = match identity {
            crate::identity::Identity::Text { text, .. } => vec![SystemBlock {
                kind: "text",
                text: std::borrow::Cow::Owned(text),
            }],
            crate::identity::Identity::Omitted { .. } => Vec::new(),
            crate::identity::Identity::Auto if non_anthropic => Vec::new(),
            crate::identity::Identity::Auto => vec![SystemBlock {
                kind: "text",
                text: std::borrow::Cow::Borrowed(crate::identity::CLAUDE_CODE_PREFIX),
            }],
        };
        // The rendering instructions always go last, and survive an operator
        // identity: they describe the terminal, not the model, so they are true
        // whatever the operator wrote. Dropping them with the identity text would
        // let a model emit markdown into a terminal that cannot render it.
        system.push(SystemBlock {
            kind: "text",
            text: std::borrow::Cow::Borrowed(INSTRUCTIONS_MARKDOWN),
        });
        let body = MessagesReq {
            model: &session_model,
            max_tokens: MAX_OUTPUT_TOKENS,
            system,
            tools: &tool_specs,
            messages: &active.history,
            stream: true,
        };
        // Serialised here rather than handed to `.json()` so the cache
        // breakpoints can be written into it first — and so the state turn can
        // be appended, which is why this is a `Value` rather than a struct.
        let mut encoded = serde_json::to_value(&body).map_err(|e| AgentError::Api(format!("encode: {e}")))?;
        // The live state goes on last, freshly rendered, and is never stored in
        // `history`. Two reasons it is not a historical turn: the model must
        // always see the *current* mode rather than whichever turn most
        // recently mentioned it, and appending rather than rewriting means no
        // byte before it ever changes. `mark_breakpoints` then places the
        // message breakpoint on the last real turn, before this one, so a gate
        // toggle extends the cached prefix instead of invalidating it.
        let cwd = active.session.cwd.display().to_string();
        drop(active);
        // Uniform content shapes on the history first, then the state turn,
        // then the marks. The order is load-bearing three times over.
        //
        // Shapes before the state turn, because `is_env_turn` recognises that
        // turn by its bare-string content — stabilising afterwards would turn
        // it into a block array, the check would miss it, and the breakpoint
        // would land on the one message that is rebuilt every request. That is
        // precisely the failure this design exists to avoid.
        //
        // Shapes before the marks, because `mark_breakpoints` promotes a string
        // to a block array to attach one, and it only marks the last real turn
        // — so without this the same message would change shape once it stopped
        // being last. See `cache::stabilise_shapes`.
        //
        // Emptiness before the marks too, and for the same reason: a turn this
        // removes must not be the turn a breakpoint was just attached to. It is
        // the last step before the wire that can drop a message, so it catches
        // history from every source — resumed from disk, rebuilt from a
        // transcript, or built live — and the API's answer to an empty message
        // is a 400 that ends the turn. See `cache::prune_empty_content`.
        let reshaped = cache::stabilise_shapes(&mut encoded);
        let pruned = cache::prune_empty_content(&mut encoded);
        cache::append_env_turn(&mut encoded, &cwd, self.gate().as_str());
        let breakpoints = cache::mark_breakpoints(&mut encoded);
        log::debug!(
            "relay request: {reshaped} messages reshaped, {pruned} empty blocks or turns pruned, \
             {breakpoints} cache breakpoints"
        );
        // Serialised once and reused as the request body. Two reasons it is not
        // handed to `.json()`: the cache breakpoints have to be written into the
        // value first, and the byte length is needed for the spinner's estimate —
        // measuring with a separate `to_vec` would serialise the whole payload
        // twice, which for a long session is the largest object the process
        // handles.
        let payload = serde_json::to_vec(&encoded).map_err(|e| AgentError::Api(format!("encode: {e}")))?;
        let payload_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        // Published after every mutation of `encoded` (state turn and breakpoints
        // both add bytes), or the figure would be short. Cleared by the guard on
        // every exit path, so a stale estimate cannot outlive its request.
        self.inflight_input_bytes
            .store(payload_bytes, std::sync::atomic::Ordering::Relaxed);
        let _clear = InflightGuard(&self.inflight_input_bytes);
        // And forget what the *previous* request reported, so the row cannot price this request
        // with the last one's numbers. The reasoning buffer is cleared in the same place and for
        // the same reason — see `clear_live`, which says why the two are one call.
        self.clear_live();
        // Sent through the retry helper rather than straight: a 429 here used
        // to end the turn, and a rate limit is a statement about timing, not
        // about the request. The closure rebuilds the request per attempt
        // because a `RequestBuilder` is consumed by `send`, and resending the
        // identical body is exactly what a retry means.
        //
        // Streamed, so the model's reasoning is on screen while it is being
        // written rather than arriving all at once at the end. The reply is put
        // back together into the same `MessagesResp` the buffered call returned,
        // so everything after this line is unchanged.
        send_streaming(
            || {
                let mut attempt = self
                    .http
                    .post(relay.messages_url())
                    .header("x-api-key", relay.token())
                    // `.body()` rather than `.json()` because the bytes are already
                    // serialised; the header matches what `.json()` would set, and is
                    // what the proxy forwards upstream.
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .header("anthropic-version", claude_api::ANTHROPIC_VERSION);
                if let Some((client_id, client_secret)) = relay.cloudflare_access() {
                    attempt = attempt
                        .header("CF-Access-Client-Id", client_id)
                        .header("CF-Access-Client-Secret", client_secret);
                }
                // A `Vec` clone is a memcpy, and only happens on a retry — cheaper
                // than re-serialising, and reqwest needs owned bytes per attempt.
                attempt.body(payload.clone())
            },
            &self.reasoning,
            &self.live,
        )
        .await
    }

    /// Same turn, different wire: translate our Anthropic-shaped
    /// history into an `OpenAI` chat-completions request against the
    /// configured endpoint, and fold the response back into
    /// `MessagesResp`. No OAuth dance — the API token is static.
    async fn call_openai_compat(&self, cfg: &crate::openai::Config) -> Result<MessagesResp, AgentError> {
        let active = self.active.read().await;
        // Static only. The live state is *not* appended here: this wire has one
        // system string rather than a list, so putting the state in it would
        // put mutable bytes at the very front — the defect this change exists
        // to remove. `build_request` appends it after the history instead, for
        // the same reason it goes last on the relay wire.
        let system = INSTRUCTIONS_MARKDOWN.to_owned();
        let tool_specs = self.tools.specs().to_vec();
        let state = cache::env_text(&active.session.cwd.display().to_string(), self.gate().as_str());
        let body = crate::openai::build_request(&cfg.model, &system, &state, &tool_specs, &active.history);
        drop(active);
        let resp = self
            .http
            .post(&cfg.chat_url)
            .bearer_auth(&cfg.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Api(api_error(status, &body)));
        }
        let raw = resp
            .json::<crate::openai::ChatResp>()
            .await
            .map_err(|e| AgentError::Http(format!("decode: {e}")))?;
        Ok(crate::openai::into_messages_resp(raw))
    }
}

/// Reconstruct the conversation history from a JSONL transcript.
/// Used when resuming a session in-place so the model has full context.
/// Only user/assistant turns are loaded; tool results are part of user turns.
fn rebuild_history(project_dir: &std::path::Path, id: &str) -> Vec<ApiMessage> {
    use crate::session::Block;
    use std::io::BufRead;
    let path = project_dir.join(format!("{id}.jsonl"));
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let role = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        // We hold `v` mutably so we can `mem::take` the content sub-tree
        // instead of cloning it before `from_value`. The owned `v` is
        // dropped at the end of the loop iteration anyway.
        let Some(msg) = v.get_mut("message") else { continue };
        let Some(content_val) = msg.get_mut("content") else {
            continue;
        };
        let content_owned = std::mem::take(content_val);
        match role {
            "user" => {
                // Anthropic's API rejects user turns whose content is
                // empty (`messages.N: user messages must have non-empty
                // content`). Older transcripts can contain empty turns
                // — placeholders written by aborted runs, /resume
                // bookkeeping, etc. — so skip anything that would
                // round-trip as an empty Plain string or an empty
                // Blocks array.
                let content = match content_owned {
                    serde_json::Value::String(s) if !s.is_empty() => {
                        // A Claude Code transcript records the operator's *local*
                        // commands as user turns: `/clear`, `/exit`, and the output
                        // they printed. They are not conversation — nobody said them
                        // to the model — and replayed they cost tokens, fill the
                        // window and read as though the operator had typed
                        // `<command-name>/exit</command-name>` at the agent. See
                        // [`is_local_command_noise`].
                        if is_local_command_noise(&s) {
                            continue;
                        }
                        ApiContent::Plain(s)
                    }
                    arr @ serde_json::Value::Array(_) => {
                        let blocks: Vec<Block> = serde_json::from_value(arr).unwrap_or_default();
                        // The same scaffolding can arrive as a text block.
                        let blocks: Vec<Block> = blocks
                            .into_iter()
                            .filter(|b| match b {
                                Block::Text { text } => !is_local_command_noise(text),
                                _ => true,
                            })
                            .collect();
                        if blocks.is_empty() {
                            continue;
                        }
                        ApiContent::Blocks(blocks)
                    }
                    _ => continue,
                };
                out.push(ApiMessage {
                    role: "user".into(),
                    content,
                });
            }
            "assistant" => {
                let blocks: Vec<Block> = serde_json::from_value(content_owned).unwrap_or_default();
                if !blocks.is_empty() {
                    out.push(ApiMessage {
                        role: "assistant".into(),
                        content: ApiContent::Blocks(blocks),
                    });
                }
            }
            _ => {}
        }
    }
    truncate_at_orphan_tool_use(&mut out);
    out
}

/// POST a request, waiting out a retryable failure.
///
/// Takes a builder closure rather than a built request because
/// `reqwest::RequestBuilder` is consumed by `send`, and a retry means sending
/// the same body again. Rebuilding is cheap — the body is already serialized
/// into the closure's captured value.
///
/// Returns the status and body even when the status is a failure, so each
/// caller keeps formatting its own error message; the retry policy is kept here
/// and the wording stays where it was.
///
/// A transport error is returned immediately rather than retried. That is
/// deliberate but not obviously right: a dropped connection is arguably the
/// most retryable thing there is. It is left alone because the failure mode
/// this fixes is throttling, and retrying a connection error needs a sense of
/// whether the request was received — which `reqwest` does not give and
/// guessing at would risk double-charging a turn.
/// Send a request and put its streamed reply back together.
///
/// This is the Messages wire's only sender, and it retries on the same terms the
/// old buffered path did — with one difference that streaming forces: **a retry
/// is only possible until the first event arrives.** After that the model has
/// begun answering, the caller has been shown part of it, and asking again would
/// restart a reply that is already half drawn. So a failure past that point is
/// final, which is why the retry lives around the response rather than inside
/// the read.
///
/// `live` is the agent's reasoning buffer, written after every chunk: it is the
/// only thing on screen while the model works, so it is updated as the words
/// arrive rather than once at the end. A view that only filled in when the reply
/// completed would be no better than the spinner it replaces.
///
/// `report` is the other half of the same idea and is written on the same tick — what the provider
/// has said about the request's cost. Both are taken here rather than by the caller because this
/// is the only place that feeds the assembler, and an assembler whose findings were dropped on the
/// floor would leave the row on the request's own size for the whole generation.
async fn send_streaming<F>(
    mut build: F,
    live: &std::sync::Mutex<String>,
    report: &std::sync::Mutex<LiveReport>,
) -> Result<MessagesResp, AgentError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut attempt = 1;
    loop {
        let mut resp = build().send().await.map_err(|e| AgentError::Http(e.to_string()))?;
        let status = resp.status();
        // Read the hint before consuming the body.
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(retry::parse_retry_after);

        // An unsuccessful reply is not a stream: it is a JSON error body, and it
        // is safe to retry because nothing has been shown to anyone yet.
        if !status.is_success() {
            let text = resp.text().await.map_err(|e| AgentError::Http(e.to_string()))?;
            if retry::is_retryable(status.as_u16()) && attempt < retry::MAX_ATTEMPTS {
                let wait = retry::delay_for(attempt + 1, retry_after);
                log::warn!(
                    "provider returned {status}; waiting {wait:?} before attempt {} of {}",
                    attempt + 1,
                    retry::MAX_ATTEMPTS
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }
            return Err(AgentError::Api(api_error(status, &text)));
        }

        // A provider that does not stream answers a `stream: true` request with
        // the ordinary whole body. Reading it as events would fail on the first
        // line, so the content type decides — the same test the proxy makes
        // before it pumps a reply through its translator.
        let streaming = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ctype| ctype.contains("event-stream"));
        if !streaming {
            let text = resp.text().await.map_err(|e| AgentError::Http(e.to_string()))?;
            return serde_json::from_str::<MessagesResp>(&text).map_err(|e| {
                AgentError::Api(format!(
                    "the reply from the relay did not decode: {e}\n  first bytes: {}",
                    truncate(&text, 400)
                ))
            });
        }

        let mut asm = crate::stream::Assembler::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    asm.feed(&chunk).map_err(|e| AgentError::Api(format!("stream: {e}")))?;
                    // Both published in one scope so the two locks are released before anything
                    // else in this arm runs: they are read several times a second by the redraw
                    // tick, and holding either across a branch is contention for nothing.
                    {
                        // `clone_into` rather than assignment: this runs for every chunk of
                        // every reply, and reusing the buffer the lock already holds turns a fresh
                        // allocation per chunk into a copy into memory that is there anyway.
                        asm.reasoning().clone_into(&mut live.lock().expect("reasoning mutex"));
                        // The same tick carries what the chunk said about the request's cost. Read
                        // from the assembler rather than from the frame directly, so the parsing
                        // rule lives in one place — and so a provider whose usage arrives in an
                        // unexpected shape shows up in this display and the totals alike, or in
                        // neither.
                        *report.lock().expect("live mutex") = LiveReport {
                            started: asm.started(),
                            usage: asm.usage().copied().unwrap_or_default(),
                            output_bytes: asm.output_bytes(),
                            model: asm.model().map(ToOwned::to_owned),
                        };
                    }
                    // `message_stop` has arrived, so the reply is complete. Stop
                    // reading rather than waiting for the server to close the
                    // connection, which it may hold open for its own reasons and
                    // which would otherwise delay the answer on screen.
                    if asm.is_done() {
                        break;
                    }
                }
                // The body ended. Whether that is the whole reply is
                // `finish`'s question, not this loop's.
                Ok(None) => break,
                // Mid-reply, a dropped connection is not retried: part of the
                // answer has been seen, and re-asking would restart it. A
                // transport error was never retried on this wire even when the
                // reply was buffered, so nothing here is a regression.
                Err(e) => return Err(AgentError::Http(e.to_string())),
            }
        }
        return asm.finish().map_err(AgentError::Api);
    }
}

/// Zeroes an in-flight byte counter when dropped.
///
/// A guard rather than a clear at the end of the function because the request
/// can end four ways — success, a decode failure, a retry giving up, or a
/// cancellation — and the counter must be zero in all of them. Clearing
/// explicitly would mean four call sites, each of which could be forgotten, and
/// a stale non-zero value would show a token count for a request that is no
/// longer running.
struct InflightGuard<'a>(&'a std::sync::atomic::AtomicU64);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Clip a response body to at most `max` bytes for an error message, so a
/// multi-megabyte body can't bury the daemon's own log. Backs off to a
/// `char` boundary so a multi-byte character at the cut is never split.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A copy of `content` with escapes removed from its text blocks only.
///
/// For the transcript, which is shared and therefore read by clients that
/// interpret nothing — tab-atelier renders chat bubbles from it. Only `text` is
/// rewritten: tool-use ids are what the upstream matches results against, and a
/// thinking block's signature is checked against the reasoning it signs, so
/// either one edited here would turn a display fix into a rejected request.
fn plain_content(content: &[Block]) -> Vec<Block> {
    content
        .iter()
        .map(|block| match block {
            Block::Text { text } => Block::Text {
                text: ansi::strip_owned(text.clone()),
            },
            // Clone as-is: tool_use, tool_result and thinking all carry
            // load-bearing bytes that must survive verbatim.
            other => other.clone(),
        })
        .collect()
}

/// The reply as any sink should receive it.
///
/// The instruction is a request, not a guarantee. A model can ignore it, half-apply
/// it, or have learned the old instruction and kept emitting escapes — a session
/// resumed from a transcript written before this changed is exactly that case. So
/// escapes are removed on the way out rather than trusted not to arrive.
///
/// This used to pass escapes through when a terminal was attached, on the reasoning
/// that the terminal would interpret them. A live session showed the flaw: what a
/// sink *declares* is not what it does. The REPL draws through ratatui, which emits
/// its own styling, so an escape from the model is never wanted — and the other
/// sinks never wanted one at all.
///
/// Free rather than a method because it no longer depends on the agent: there is one
/// behaviour for every sink, which is the point of the change. Both channels are
/// filtered, including the reasoning, which the socket still sends even though the
/// REPL no longer prints it.
fn for_sink(text: String) -> String {
    ansi::strip_owned(text)
}

/// Whether a transcript entry is Claude Code's scaffolding for a local command,
/// rather than something the operator said to the model.
///
/// Found in a live payload: a session resumed from a Claude Code transcript carried
/// four user turns nobody wrote —
///
/// ```text
/// <local-command-caveat>Caveat: …</local-command-caveat>
/// <command-name>/clear</command-name>
/// <local-command-stdout>Bye!</local-command-stdout>
/// ```
///
/// — which the model then read as things the operator had told it. They cost tokens
/// on every request, since history is re-sent each turn, and they are confusing in
/// exactly the way a stray `/exit` in a prompt is confusing.
///
/// Matched at the start of the text, not anywhere in it, so a message that *quotes*
/// one of these markers is left alone: the operator quoting a command is talking to
/// the model, and only the scaffolding itself is not.
fn is_local_command_noise(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "<local-command-caveat>",
        "<local-command-stdout>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
    ];
    let trimmed = text.trim_start();
    MARKERS.iter().any(|marker| trimmed.starts_with(marker))
}

/// Anthropic's API requires every `tool_use` in an assistant turn to
/// be answered by a matching `tool_result` in the very next user
/// turn. Old transcripts can dangle — process killed mid-tool, /resume
/// after a crash, etc. — leaving an assistant turn whose `tool_use`
/// ids are never satisfied. Truncate the loaded history at the first
/// such orphan so the rebuild remains a valid wire sequence.
fn truncate_at_orphan_tool_use(out: &mut Vec<ApiMessage>) {
    use crate::session::Block;
    use std::collections::HashSet;

    let mut keep = out.len();
    let mut i = 0;
    while i < out.len() {
        if out[i].role != "assistant" {
            i += 1;
            continue;
        }
        let uses: HashSet<String> = match &out[i].content {
            ApiContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let Block::ToolUse { id, .. } = b {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            ApiContent::Plain(_) => HashSet::new(),
        };
        if uses.is_empty() {
            i += 1;
            continue;
        }
        let Some(next) = out.get(i + 1) else {
            keep = i;
            break;
        };
        if next.role != "user" {
            keep = i;
            break;
        }
        let results: HashSet<String> = match &next.content {
            ApiContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let Block::ToolResult { tool_use_id, .. } = b {
                        Some(tool_use_id.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            ApiContent::Plain(_) => HashSet::new(),
        };
        if !uses.is_subset(&results) {
            keep = i;
            break;
        }
        i += 2;
    }
    out.truncate(keep);
}

/// Build the label the spinner shows while a tool runs, as `Name(subject)`.
///
/// `Read(src/handlers.rs)` rather than `Read: src/handlers.rs`: the name and its subject read as
/// one token, and the `Thinking - ` the status row puts in front already separates the tool from
/// everything else.
fn tool_status_label(name: &str, input: &serde_json::Value) -> String {
    tool_subject(input).map_or_else(|| name.to_owned(), |subject| format!("{name}({subject})"))
}

/// The value worth naming for a tool call, or `None` for one that its own name already describes.
///
/// An allow-list of fields rather than a scan of whatever the call happens to carry, because
/// which field identifies a call is a judgement about the tool and not something derivable from
/// its schema. `path` or `command` says what the call *does*; `content`, `note` or `prompt` is
/// the payload it carries, and `Write(the file is now updated)` would name the text instead of
/// the file. Nothing is shown for a call with none of these fields — `ListAgents` is the real
/// case, since its arguments are absent by design.
///
/// Ordered by how much each field identifies the call: where it points beats what it does, and
/// both beat what it filters on. So `SSH(dc18…)` names the machine rather than the command, and
/// a `PHPUnit` run against a path shows the path rather than its test filter.
fn tool_subject(input: &serde_json::Value) -> Option<String> {
    const IDENTIFYING: [&str; 8] = [
        "path",
        "host",
        "command",
        "target",
        "task",
        "action",
        "testsuite",
        "filter",
    ];
    for field in IDENTIFYING {
        let Some(value) = input.get(field).and_then(serde_json::Value::as_str) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        return Some(if field == "path" {
            short_path(value)
        } else {
            clip(value, MAX_SUBJECT_CHARS)
        });
    }
    None
}

/// How much of a command, task or filter to show before eliding. The status row shares its width
/// with the spinner and the token counts, so the label has to stay a glance rather than a read.
const MAX_SUBJECT_CHARS: usize = 60;

/// A path reduced to its last two components, so a deep path cannot overflow the status row.
///
/// `file_name` plus the parent's `file_name` gets both without collecting the whole path.
fn short_path(path: &str) -> String {
    let parsed = std::path::Path::new(path);
    match (parsed.file_name(), parsed.parent().and_then(std::path::Path::file_name)) {
        (Some(file), Some(parent)) => {
            format!("{}/{}", parent.to_string_lossy(), file.to_string_lossy())
        }
        (Some(file), None) => file.to_string_lossy().into_owned(),
        _ => path.to_owned(),
    }
}

/// The first `max` characters, with an ellipsis when there are more.
///
/// Counted in characters, not bytes. The code this replaces compared `cmd.len()` — bytes —
/// against a 60-*character* truncation, so a command containing any non-ASCII character reported
/// itself as over-long while its text still fitted.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).chain(std::iter::once('…')).collect()
}

// --- API wire types --------------------------------------------------------

#[derive(Serialize)]
struct MessagesReq<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    tools: &'a [serde_json::Value],
    messages: &'a [ApiMessage],
    /// Ask for the reply as server-sent events.
    ///
    /// Always true. The unstreamed shape is not kept as an alternative: a
    /// streamed reply that has been reassembled is the same
    /// [`MessagesResp`] either way (see [`crate::stream`]), so the only
    /// difference is that this way the reasoning can be watched as it arrives
    /// instead of all at once at the end.
    stream: bool,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: std::borrow::Cow<'a, str>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct MessagesResp {
    pub content: Vec<Block>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Deserialize, Debug, Clone, Copy, Default)]
// Every field ends in `_tokens` because they are the names the provider sends: serde matches on
// them, so they cannot be tidied into something shorter without an alias for each. The lint is
// about a *domain* type whose fields drift into a shared suffix, which is not this.
#[allow(clippy::struct_field_names)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens served from the prompt cache rather than sent fresh.
    ///
    /// Kept because they are priced differently — roughly a tenth of an input token — and the
    /// client sends three cache breakpoints on every request, so a cost computed from input and
    /// output alone is wrong on nearly every turn. The relay passes these through; the client
    /// was simply not reading them.
    #[serde(default, alias = "cache_read_tokens", alias = "cached_tokens")]
    pub cache_read_input_tokens: u64,
    /// Tokens written into the prompt cache, which are billed above the normal input rate.
    #[serde(default, alias = "cache_write_tokens")]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// The four kinds as one value, for the cost arithmetic.
    #[must_use]
    pub const fn tokens(&self) -> crate::cost::Tokens {
        crate::cost::Tokens {
            // `input_tokens` counts everything sent, cached reads included on some providers, so
            // what is charged at the full input rate is the remainder. Subtracting is the
            // conservative reading: it cannot double-charge a cached token the way treating the
            // field as fresh input would.
            input: self.input_tokens.saturating_sub(self.cache_read_input_tokens),
            output: self.output_tokens,
            cache_read: self.cache_read_input_tokens,
            cache_write: self.cache_creation_input_tokens,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct ApiMessage {
    pub role: String,
    pub content: ApiContent,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum ApiContent {
    Plain(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("relay: {0}")]
    Relay(#[from] crate::relay::RelayError),
    #[error("api: {0}")]
    Api(String),
    #[error("http: {0}")]
    Http(String),
    #[error("transcript: {0}")]
    Transcript(#[from] crate::session::SessionError),
    /// Carries the limit for the same reason the warning does: a message that
    /// names a round count the operator cannot act on is worse than naming none.
    #[error("tool loop hit the {max_rounds}-round cap with no text to return (set CATBUS_MAX_ROUNDS to raise it)")]
    TooManyRounds { max_rounds: u32 },
    #[error("cancelled by user")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thinking block survives `plain_content` byte for byte, signature
    /// included.
    ///
    /// Verified against three real transcripts first — 44,000-odd entries, 0 that
    /// failed to parse, 0 signatures lost and 0 text mismatches — and pinned here
    /// with the shape those files use, since a test cannot read the operator's
    /// home. The signature is the part that matters: a provider that implements
    /// thinking verifies it, so a block that arrives with its text and without its
    /// signature is worse than one that never arrived.
    #[test]
    fn thinking_survives_the_block_conversion_intact() {
        let raw: Vec<crate::session::Block> = serde_json::from_value(serde_json::json!([
            { "type": "thinking", "thinking": "  weigh the options\n  then commit  ", "signature": "sig-abc" },
            { "type": "text", "text": "Here is the schema." },
        ]))
        .expect("parses");

        let kept = plain_content(&raw);
        assert_eq!(kept.len(), 2, "both blocks kept: {kept:#?}");
        // Whitespace is the model's, not ours: only the empty case is trimmed.
        assert_eq!(
            serde_json::to_value(&kept[0]).expect("serialises"),
            serde_json::json!({
                "type": "thinking",
                "thinking": "  weigh the options\n  then commit  ",
                "signature": "sig-abc",
            }),
            "the thinking and its signature must round-trip unchanged"
        );
    }

    /// A thinking block whose text is empty still round-trips, signature and all.
    ///
    /// This is the shape 41,000 times over in the local transcripts: a session
    /// where the provider kept the signature and dropped the text. Keeping the
    /// block here is deliberate — dropping a block that names a signature would
    /// change the turn — and it is `cache::prune_empty_content` that removes it
    /// before it can reach the API.
    #[test]
    fn an_empty_thinking_block_keeps_its_signature_for_the_prune_to_see() {
        let raw: Vec<crate::session::Block> = serde_json::from_value(serde_json::json!([
            { "type": "thinking", "thinking": "", "signature": "sig-abc" },
        ]))
        .expect("parses");
        let kept = plain_content(&raw);
        assert_eq!(
            serde_json::to_value(&kept[0]).expect("serialises"),
            serde_json::json!({ "type": "thinking", "thinking": "", "signature": "sig-abc" })
        );
    }

    /// Write a transcript in the shape the agent writes, under a temp project
    /// dir, and hand back (dir, id).
    fn transcript(lines: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = "00000000-0000-0000-0000-0000000000ff".to_owned();
        std::fs::write(dir.path().join(format!("{id}.jsonl")), lines).expect("write transcript");
        (dir, id)
    }

    /// The resume path against the failure that produced
    /// `messages.6: all messages must have non-empty content`.
    ///
    /// A transcript entry whose content is a single `thinking` block with no
    /// thinking in it parses fine — `Block` has a field for the text, and an
    /// empty string is a valid one — so the reader keeps the turn, because it
    /// describes what the transcript says. It is the request pipeline that has
    /// to drop it, because only there is it known to be something the API
    /// refuses. This asserts both halves are still wired together: the reader
    /// keeping the turn is what makes the pipeline's job load-bearing.
    #[test]
    fn a_resumed_turn_of_an_empty_thinking_block_is_pruned_from_the_request() {
        let (dir, id) = transcript(concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"and now?"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"sig"}]}}"#,
            "\n",
        ));

        let history = rebuild_history(dir.path(), &id);
        assert_eq!(history.len(), 2, "the reader keeps what the transcript says");

        // Exactly the pipeline's order up to the prune.
        let mut body = serde_json::json!({ "messages": history });
        crate::cache::stabilise_shapes(&mut body);
        assert_eq!(crate::cache::prune_empty_content(&mut body), 2);

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1, "the turn that said nothing is gone");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "and now?");
    }

    /// The shape the live loop hands over when a model replies with a thinking
    /// block and a tool call: the turn is real, only the thinking is empty, and
    /// the `tool_use` must survive — the `tool_result` answering it is in the
    /// very next turn, and an unmatched pair is its own 400.
    ///
    /// The transcript is the live one, in order. A lone `tool_use` turn would
    /// be dropped by `truncate_at_orphan_tool_use` before this pass ever ran,
    /// so the answer to the call has to be here or the test proves nothing.
    #[test]
    fn an_empty_thinking_block_does_not_take_a_tool_call_with_it() {
        let (dir, id) = transcript(concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"  ","signature":"sig"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":["#,
            r#"{"type":"tool_result","tool_use_id":"t1","content":"the file"}]}}"#,
            "\n",
        ));

        let mut body = serde_json::json!({ "messages": rebuild_history(dir.path(), &id) });
        assert_eq!(crate::cache::prune_empty_content(&mut body), 1);

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "only the block goes, not a turn");
        let blocks = messages[1]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1, "the empty thinking block is gone");
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "t1");
        assert_eq!(
            messages[2]["content"][0]["tool_use_id"], "t1",
            "and the call is still answered"
        );
    }

    /// Local-command scaffolding is not conversation.
    ///
    /// Taken from a live request's payload, which carried four user turns nobody
    /// had written: a `/clear`, an `/exit`, their caveats and their output. The
    /// model read them as things the operator had said.
    #[test]
    fn local_command_scaffolding_is_not_replayed_as_a_user_turn() {
        for noise in [
            "<local-command-caveat>Caveat: the messages below…</local-command-caveat>",
            "<command-name>/clear</command-name>",
            "  <command-name>/exit</command-name>\n<command-message>exit</command-message>",
            "<local-command-stdout>Bye!</local-command-stdout>",
        ] {
            assert!(is_local_command_noise(noise), "should be skipped: {noise}");
        }

        // A real message is kept, including one that *mentions* a command — the
        // operator quoting a command is talking to the model.
        for real in [
            "explain what /clear does",
            "the output said <local-command-stdout>Bye!</local-command-stdout> but",
            "read the parser",
        ] {
            assert!(!is_local_command_noise(real), "should be kept: {real}");
        }
    }

    /// A provider error is shown as the provider's own sentence.
    ///
    /// The body is the one a live session hit, verbatim: a 503 with the useful
    /// sentence inside a JSON envelope, cut off mid-string by the old truncation and
    /// printed with its braces, so nothing suggested it was worth reading.
    #[test]
    fn a_provider_error_is_unwrapped_into_a_sentence() {
        let body = r#"{"error":{"message":"Service is too busy. We advise users to temporarily switch to alternative LLM API service providers.","type":"service_unavailable_error"}}"#;
        let said = api_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body);

        assert!(said.starts_with("503 Service Unavailable"), "{said}");
        assert!(
            said.contains("Service is too busy"),
            "the provider's own sentence is the useful part: {said}"
        );
        assert!(
            said.contains("service_unavailable_error"),
            "and its type names itself in the provider's vocabulary: {said}"
        );
        // No envelope survives into what a person reads.
        assert!(!said.contains('{') && !said.contains('"'), "unwrapped: {said}");
        // The provider's sentence is printed whole, advice included: trimming an
        // arbitrary message is string surgery on text whose shape we do not own. What
        // is added is the fact the operator cannot otherwise see — that the retries
        // have already happened — and deliberately not advice, which would argue with
        // the sentence right above it.
        assert!(
            said.contains("switch to alternative LLM API service providers"),
            "the provider's own words are kept whole: {said}"
        );
        assert!(
            said.contains("retried, and the provider was still failing"),
            "and the fact of the retries is added: {said}"
        );
    }

    /// The row's state before the provider has said anything: the request's own measured size,
    /// marked as the estimate it is.
    ///
    /// This is the figure the row showed before any of this existed, and it has to survive
    /// unchanged — a session whose relay never reports usage, or a model that answers with a
    /// non-streamed body, gets no provider counts at all, and a row that went blank for them would
    /// be a regression from showing an estimate.
    #[test]
    fn before_the_reply_opens_the_only_figure_is_the_requests_own_size() {
        let live = live_figures(LiveReport::default(), 400_000).expect("something to show");
        assert!(!live.reported, "it is arithmetic, not a count");
        assert!(live.output_estimated, "and no output has arrived");
        assert_eq!(live.usage.input_tokens, 100_000, "400 kB at 4 bytes a token");
        assert_eq!(live.usage.output_tokens, 0);
    }

    /// Nothing measured and nothing reported is nothing to show, which is the row between turns.
    #[test]
    fn a_request_that_has_not_been_sized_shows_nothing() {
        assert!(live_figures(LiveReport::default(), 0).is_none());
    }

    /// Once the reply opens, the input count is the provider's own and the output is counted from
    /// what has arrived — the asymmetry `Live` carries, and the reason the price on the row is
    /// still marked as an estimate after the counts stop being one.
    #[test]
    fn an_open_reply_takes_the_providers_input_count_and_counts_its_output() {
        let report = LiveReport {
            started: true,
            usage: Usage {
                input_tokens: 54_321,
                ..Usage::default()
            },
            output_bytes: 8_000,
            model: Some("claude-sonnet-4-6".to_owned()),
        };
        let live = live_figures(report, 900_000).expect("something to show");
        assert!(live.reported, "the input count is the provider's");
        assert_eq!(
            live.usage.input_tokens, 54_321,
            "the estimate is superseded, not blended with"
        );
        assert_eq!(live.usage.output_tokens, 2_000, "8 kB of reply at 4 bytes a token");
        assert!(live.output_estimated, "the output half is still a guess");
        assert_eq!(
            live.model.as_deref(),
            Some("claude-sonnet-4-6"),
            "and the model travels through, because the price depends on it"
        );
    }

    /// The closing `message_delta` is the only frame with the real output count, and it wins
    /// outright over the bytes received — the two are not averaged or added.
    #[test]
    fn the_providers_output_count_supersedes_the_byte_estimate() {
        let report = LiveReport {
            started: true,
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 640,
                ..Usage::default()
            },
            output_bytes: 8_000,
            ..LiveReport::default()
        };
        let live = live_figures(report, 0).expect("something to show");
        assert_eq!(
            live.usage.output_tokens, 640,
            "the count, not the 2,000 the bytes imply"
        );
        assert!(!live.output_estimated);
        assert!(live.reported);
    }

    /// A stream that produced content without a `message_start` still shows something: the input
    /// side falls back to the request's size, and the reply's own bytes are still counted. Better a
    /// figure marked as an estimate than a blank row on a provider that opened with a frame this
    /// build did not recognise.
    #[test]
    fn content_without_a_message_start_still_reports_output() {
        let report = LiveReport {
            started: false,
            usage: Usage::default(),
            output_bytes: 4_000,
            ..LiveReport::default()
        };
        let live = live_figures(report, 40_000).expect("something to show");
        assert!(!live.reported, "nothing was confirmed, so the figures are estimates");
        assert_eq!(live.usage.input_tokens, 10_000, "the request's own size");
        assert_eq!(live.usage.output_tokens, 1_000);
    }

    /// The two counts are the four kinds the pricing applies to, which is what makes the price on
    /// the row the same arithmetic as the one on the totals line.
    #[test]
    fn the_live_counts_are_the_four_kinds_the_catalog_prices() {
        let report = LiveReport {
            started: true,
            usage: Usage {
                input_tokens: 900,
                output_tokens: 100,
                cache_read_input_tokens: 5_000,
                cache_creation_input_tokens: 400,
            },
            output_bytes: 0,
            ..LiveReport::default()
        };
        let live = live_figures(report, 0).expect("something to show");
        let tokens = live.tokens();
        assert_eq!(tokens.input, 0, "900 sent of which 900 cached reads");
        assert_eq!(tokens.cache_read, 5_000);
        assert_eq!(tokens.cache_write, 400);
        assert_eq!(tokens.output, 100);
    }

    /// A hint only where the retry layer has already given up.
    #[test]
    fn only_a_retryable_status_mentions_retrying() {
        let body = r#"{"error":{"message":"bad tool schema","type":"invalid_request_error"}}"#;
        let said = api_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(said.contains("bad tool schema"), "{said}");
        assert!(
            !said.contains("retried"),
            "a 400 is not retried, so the hint would be a lie: {said}"
        );
    }

    /// The other two envelopes, and a body that is not JSON at all.
    ///
    /// A shape we do not recognise is still information: printing it beats inventing
    /// an explanation for it.
    #[test]
    fn an_unrecognised_body_is_shown_rather_than_explained() {
        // `error` as a bare string.
        let said = api_error(reqwest::StatusCode::UNAUTHORIZED, r#"{"error":"no token"}"#);
        assert!(said.contains("no token"), "{said}");

        // A top-level `message`, from a proxy that unwraps one level less.
        let said = api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, r#"{"message":"slow down"}"#);
        assert!(said.contains("slow down"), "{said}");
        assert!(said.contains("retried"), "429 is retryable too: {said}");

        // Not JSON: shown as it came, bounded.
        let said = api_error(reqwest::StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>");
        assert!(said.contains("502 Bad Gateway"), "{said}");
        assert!(said.starts_with("502 Bad Gateway"), "{said}");

        // Empty: still a sentence, not a dangling colon.
        let said = api_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "");
        assert!(said.starts_with("500 Internal Server Error"), "{said}");
    }

    /// A very long message is cut, so one provider response cannot fill the screen.
    #[test]
    fn a_long_message_is_bounded() {
        let long = "x".repeat(5_000);
        let body = format!(r#"{{"error":{{"message":"{long}"}}}}"#);
        let said = api_error(reqwest::StatusCode::BAD_GATEWAY, &body);
        assert!(said.len() < 1_000, "bounded, got {}", said.len());
    }
    /// A tool label names the call as `Name(subject)`, with the subject shortened so a deep path
    /// cannot overflow the status row.
    #[test]
    fn a_tool_label_names_the_subject() {
        let read = serde_json::json!({"path": "crates/catbus-agent/src/tools/mod.rs"});
        assert_eq!(tool_status_label("Read", &read), "Read(tools/mod.rs)");
        // A bare filename has no parent component to show.
        assert_eq!(
            tool_status_label("Read", &serde_json::json!({"path": "Cargo.toml"})),
            "Read(Cargo.toml)"
        );
        // Bash shows what is running, not a path.
        assert_eq!(
            tool_status_label("Bash", &serde_json::json!({"command": "cargo test"})),
            "Bash(cargo test)"
        );
        // Spawn and Delegate name what they act on.
        assert_eq!(
            tool_status_label("Spawn", &serde_json::json!({"task": "review the diff"})),
            "Spawn(review the diff)"
        );
        assert_eq!(
            tool_status_label("Delegate", &serde_json::json!({"target": "Reviewer", "prompt": "x"})),
            "Delegate(Reviewer)"
        );
        // Git names its action, since that is what the call does.
        assert_eq!(
            tool_status_label("Git", &serde_json::json!({"action": "commit", "files": []})),
            "Git(commit)"
        );
    }

    /// A subject is what the call acts *on*, so where a call points beats what it does — and a
    /// payload is never chosen. Showing the payload would produce labels like
    /// `Write(the file is now updated)`, which name the text instead of the file.
    #[test]
    fn a_payload_is_never_the_subject() {
        // SSH: the machine matters more than the command being sent to it.
        let ssh = serde_json::json!({"action": "command", "host": "dc18.servers.example.org", "command": "uptime"});
        assert_eq!(tool_status_label("SSH", &ssh), "SSH(dc18.servers.example.org)");

        // A Write whose only argument is the text it is writing says nothing: the path is what
        // identifies the call, and without it the tool's own name is the honest label.
        let write = serde_json::json!({"content": "the file is now updated"});
        assert_eq!(tool_status_label("Write", &write), "Write");

        // An empty string is not a subject either — it would render an empty pair of brackets.
        assert_eq!(tool_status_label("Read", &serde_json::json!({"path": ""})), "Read");
        // And a tool with no arguments at all is just its name.
        assert_eq!(tool_status_label("ListAgents", &serde_json::json!({})), "ListAgents");
    }

    /// The spinner's activity text puts the tool beside "Thinking" rather than replacing it, so
    /// the operator can see that the model is still working and what it is working on.
    #[test]
    fn the_status_row_shows_the_tool_while_thinking() {
        let read = serde_json::json!({"path": "src/handlers.rs"});
        assert_eq!(crate::statusline::activity_label("thinking"), "Thinking");
        assert_eq!(
            crate::statusline::activity_label(&tool_status_label("Read", &read)),
            "Thinking - Read(src/handlers.rs)"
        );
    }

    /// A long command is clipped with an ellipsis, counted in characters so a non-ASCII character
    /// neither splits nor claims to be longer than it is.
    #[test]
    fn a_long_subject_is_clipped_by_characters() {
        let long = "x".repeat(200);
        let label = tool_status_label("Bash", &serde_json::json!({"command": long}));
        // `Bash(` + 60 characters + `…` + `)`
        assert_eq!(label.chars().count(), 5 + 60 + 1 + 1);
        assert!(label.ends_with("…)"));
        assert!(label.starts_with("Bash(x"));

        // Exactly at the limit: no ellipsis, since nothing was left out.
        let exact = "y".repeat(60);
        assert_eq!(
            tool_status_label("Bash", &serde_json::json!({"command": exact})),
            format!("Bash({exact})")
        );

        // One multi-byte character over: counted as one character, not its byte length.
        let multibyte = "é".repeat(61);
        let clipped = clip(&multibyte, 60);
        assert_eq!(clipped.chars().count(), 61, "60 characters plus the ellipsis");
        assert!(clipped.ends_with('…'));
        // 61 `é` is 122 bytes, so a byte-based clip would have mangled the text.
        assert!(clipped.starts_with('é'));
    }

    /// A path is shortened to its last two components for display, but only for display: the
    /// label is never used to open anything.
    #[test]
    fn a_path_is_shortened_to_two_components() {
        assert_eq!(short_path("a/b/c/d.rs"), "c/d.rs");
        assert_eq!(short_path("d.rs"), "d.rs");
        assert_eq!(short_path("/x/y.rs"), "x/y.rs");
        // A trailing slash is normalised away by `Path`, so a directory path still shortens to
        // two components rather than echoing its own separator back.
        assert_eq!(short_path("a/b/"), "a/b");
        assert_eq!(short_path("a/b/c/"), "b/c");
    }
}
