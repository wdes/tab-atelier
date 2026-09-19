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
//! * The first system block **must** start with the literal Claude
//!   Code identifier; the upstream rejects the request otherwise, and
//!   the proxy forwards system blocks untouched.
//! * Our own instructions go in the second system block, and both blocks are
//!   static — the working directory and permission mode are a trailing turn,
//!   so that changing either does not invalidate the cached prefix.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use std::fmt::Write as _;

use crate::relay::Relay;
use crate::session::{self, Block, Session};
use crate::tools;
use crate::{ansi, cache, guard, retry};

/// Identifier the server requires at the start of the first system
/// block on every Messages call. The proxy forwards system blocks
/// through untouched, so the client is still the one that has to send it.
const CLAUDE_CODE_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Sticking to a non-thinking, non-1M-context model keeps the bring-up
/// surface small. Both can be swapped via `/model` later.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Output-token ceiling for a Messages call.
///
/// A named constant rather than a literal at the request site, because the
/// truncation warning below quotes it: writing the number in two places is how
/// a message comes to promise a limit the request does not use, which is exactly
/// what happened to the round-cap warning that claimed "32" while the default
/// was 200.
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// Static portion of the second system block, when a terminal will render it.
///
/// Both system blocks are now `&'static str`: the working directory and the
/// permission mode used to be formatted into a block here, which put mutable
/// bytes in the middle of the prompt and invalidated everything after them on
/// every toggle. They travel in a trailing turn instead — see [`cache`].
///
/// Nothing session-specific may be added to this text. If it can change while
/// the agent runs, it belongs in the env turn.
const INSTRUCTIONS_TERMINAL: &str = "Your text replies are rendered directly in a terminal emulator \
    that supports ANSI colour and formatting — use ANSI SGR escapes \
    (bold, colours, etc.) to make output readable. Do NOT use \
    markdown — no asterisks, no backtick fences, no hashes. \
    Use ANSI instead: \x1b[1m for bold, \x1b[32m for green, \
    \x1b[33m for yellow, \x1b[31m for red, \x1b[36m for cyan, \
    \x1b[0m to reset.";

/// The same slot when *no* terminal will render the answer.
///
/// One session is read by several clients and only some have a terminal: the
/// REPL prints to one, but the socket, the transcript an API client mirrors,
/// and a phone showing the same conversation do not. Asking for SGR there is
/// asking for literal `[1m` in the text — the emphasis arrives as noise. So the
/// request has to match the sink, and this is the variant for the sinks that
/// interpret nothing.
///
/// Markdown stays out for the same reason it is out of the terminal text:
/// plain prose is what reads correctly with nothing in between.
const INSTRUCTIONS_PLAIN: &str = "Your text replies are shown as plain text. Do NOT use markdown or \
    ANSI escape sequences — no asterisks, no backtick fences, no hashes, \
    no colour codes: they are displayed literally and make the answer \
    harder to read. Write plain prose in short sentences. For lists, use a \
    hyphen or a number at the start of each line with no other decoration.";

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
    /// Whether the answer is written to a terminal that renders escape
    /// sequences. Chooses between [`INSTRUCTIONS_TERMINAL`] and
    /// [`INSTRUCTIONS_PLAIN`] — see [`Self::with_ansi`] for why the default is
    /// the conservative one.
    ansi: bool,
    /// Current activity description shown in the REPL spinner.
    /// `None` = idle, `Some(s)` = description of what's happening.
    pub status: std::sync::Mutex<Option<String>>,
    /// Cumulative tokens consumed across all turns in this process.
    /// Both counters accumulate monotonically and are never reset.
    pub tokens_in: std::sync::atomic::AtomicU64,
    pub tokens_out: std::sync::atomic::AtomicU64,
    /// Serialised size of the request currently in flight, in bytes, or 0 when
    /// none is. Held as a byte count rather than a token estimate so the
    /// conversion lives in one place (`statusline::estimate_input_tokens`) and
    /// can change without touching this field.
    pub inflight_input_bytes: std::sync::atomic::AtomicU64,
    /// Cancellation flag for the currently-running turn. Re-built at
    /// the start of every `run_user_prompt` so Ctrl+C only kills the
    /// in-flight request, not future ones.
    cancel: std::sync::Mutex<CancellationToken>,
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
            status: std::sync::Mutex::new(None),
            tokens_in: std::sync::atomic::AtomicU64::new(0),
            tokens_out: std::sync::atomic::AtomicU64::new(0),
            inflight_input_bytes: std::sync::atomic::AtomicU64::new(0),
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

    /// Estimated input tokens for the request currently in flight, or `None`
    /// when nothing is in flight.
    ///
    /// The estimate exists only because the server withholds `usage` until its
    /// final response, so during a download there is nothing else to show. It is
    /// set from the serialised payload's length just before sending and cleared
    /// as soon as the response arrives, so it never lingers as a stale figure
    /// once the authoritative total is available.
    #[must_use]
    pub fn inflight_input_estimate(&self) -> Option<u64> {
        match self.inflight_input_bytes.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            bytes => Some(crate::statusline::estimate_input_tokens(
                usize::try_from(bytes).unwrap_or(usize::MAX),
            )),
        }
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

    /// The instruction text matching this agent's sink.
    ///
    /// Both variants are `&'static str`, so this stays a borrow and the prompt
    /// prefix remains cacheable — a value built here would put a fresh
    /// allocation in the middle of the cached region on every turn.
    const fn instructions(&self) -> &'static str {
        if self.ansi {
            INSTRUCTIONS_TERMINAL
        } else {
            INSTRUCTIONS_PLAIN
        }
    }

    /// The reply as this agent's sink should receive it.
    ///
    /// The instruction text is a request, not a guarantee. A model can ignore
    /// it, half-apply it, or have emitted escapes before the sink changed — a
    /// session resumed by a later process writing somewhere else. Filtering on
    /// the way out means a reader with no terminal never sees `[1m`, whatever
    /// the model chose to send. When `ansi` is set the text is passed through
    /// untouched, because there the terminal *is* interpreting it.
    fn for_sink(&self, text: String) -> String {
        if self.ansi { text } else { ansi::strip_owned(text) }
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
        *self.active.write().await = ActiveSession {
            session: Arc::new(new_session),
            history,
        };
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
    pub async fn run_user_prompt(&self, text: String) -> Result<String, AgentError> {
        // Fresh token per turn so a stale cancel doesn't kill the next
        // request before it even starts. Hold the lock only long enough
        // to swap; the inner future borrows the new clone.
        let token = {
            let mut slot = self.cancel.lock().expect("cancel mutex");
            *slot = CancellationToken::new();
            slot.clone()
        };
        *self.status.lock().expect("status mutex") = Some("thinking".into());
        let result = self.run_user_prompt_inner(text, &token).await;
        *self.status.lock().expect("status mutex") = None;
        result
    }

    #[allow(clippy::too_many_lines)]
    async fn run_user_prompt_inner(&self, text: String, cancel: &CancellationToken) -> Result<String, AgentError> {
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
        // Cap on tool rounds. 200 is intentionally high — the model
        // self-terminates via end_turn long before this in normal use.
        // The env-var escape hatch exists for unusually long tasks.
        let max_rounds: u32 = std::env::var("CATBUS_MAX_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        for _ in 0..max_rounds {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            *self.status.lock().expect("status mutex") = Some("thinking".into());
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

            // Collect tool_use blocks by reference; pull any text into
            // the visible answer so the caller has *something* even
            // mid-tool-use. The borrows into `resp.content` survive the
            // tool dispatch awaits below — the history push that moves
            // `resp.content` happens *after* this borrow goes out of scope.
            let mut tool_uses: Vec<(&str, &str, &serde_json::Value)> = Vec::new();
            for block in &resp.content {
                match block {
                    Block::Text { text } => {
                        if !final_text.is_empty() {
                            final_text.push('\n');
                        }
                        final_text.push_str(text);
                    }
                    Block::ToolUse { id, name, input } => {
                        tool_uses.push((id.as_str(), name.as_str(), input));
                    }
                    // A tool result is ours, not the model's; and reasoning
                    // stays in the transcript we echo back but is not part of
                    // the answer the user asked for.
                    Block::ToolResult { .. } | Block::Thinking { .. } => {}
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
                // A reply cut off by the output limit is not a finished answer,
                // yet the branch above treats it as one: the model stopped
                // mid-sentence, so there is no tool to continue with and no
                // error to raise. Saying so is the difference between a client
                // that knows it has a fragment and one that reports `done` over
                // half a sentence — which is what happened before this check
                // existed.
                if stop_reason.as_deref() == Some("max_tokens") {
                    // `write!` rather than `push_str(&format!(..))`: the same
                    // output, without building and dropping a throwaway `String`.
                    // `write!` to a `String` is infallible, so the result is
                    // discarded rather than unwrapped.
                    let _ = write!(
                        final_text,
                        "\n\n\x1b[33m[reply cut off at the {MAX_OUTPUT_TOKENS}-token output limit — \
                         it may end mid-sentence; ask for the rest to continue]\x1b[0m"
                    );
                }
                return Ok(self.for_sink(final_text));
            }

            // Run tools, build a single user-message of tool_result
            // blocks (Messages API wants them all in one message,
            // in the same order the model produced the tool_use
            // blocks).
            let gate = self.gate();
            let mut results: Vec<Block> = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in &tool_uses {
                if cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
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
                let vetted: Option<String> = if gate.judges() && self.tools.changes_the_world(name) {
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

                let (mut content, is_error) = tokio::select! {
                    out = self.tools.dispatch(name, input, &session.cwd, gate) => {
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
            // Done with the borrows — move resp.content into history now.
            let _ = tool_uses;
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
            Ok(self.for_sink(final_text))
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
        let body = MessagesReq {
            model: DEFAULT_MODEL,
            max_tokens: MAX_OUTPUT_TOKENS,
            system: vec![
                SystemBlock {
                    kind: "text",
                    text: std::borrow::Cow::Borrowed(CLAUDE_CODE_PREFIX),
                },
                SystemBlock {
                    kind: "text",
                    text: std::borrow::Cow::Borrowed(self.instructions()),
                },
            ],
            tools: &tool_specs,
            messages: &active.history,
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
        // Sent through the retry helper rather than straight: a 429 here used
        // to end the turn, and a rate limit is a statement about timing, not
        // about the request. The closure rebuilds the request per attempt
        // because a `RequestBuilder` is consumed by `send`, and resending the
        // identical body is exactly what a retry means.
        let (status, text) = send_retrying(|| {
            let mut attempt = self
                .http
                .post(relay.messages_url())
                .header("x-api-key", relay.token())
                // `.body()` rather than `.json()` because the bytes are already
                // serialised; the header matches what `.json()` would set, and is
                // what the proxy forwards upstream.
                .header("content-type", "application/json")
                .header("anthropic-version", claude_api::ANTHROPIC_VERSION);
            if let Some((client_id, client_secret)) = relay.cloudflare_access() {
                attempt = attempt
                    .header("CF-Access-Client-Id", client_id)
                    .header("CF-Access-Client-Secret", client_secret);
            }
            // A `Vec` clone is a memcpy, and only happens on a retry — cheaper
            // than re-serialising, and reqwest needs owned bytes per attempt.
            attempt.body(payload.clone())
        })
        .await?;
        if !status.is_success() {
            return Err(AgentError::Api(format!("{status}: {}", truncate(&text, 2000))));
        }
        serde_json::from_str::<MessagesResp>(&text)
            .map_err(|e| AgentError::Api(format!("decode: {e}; body was: {}", truncate(&text, 2000))))
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
        let system = self.instructions().to_owned();
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
            return Err(AgentError::Api(format!("{status}: {body}")));
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
                    serde_json::Value::String(s) if !s.is_empty() => ApiContent::Plain(s),
                    arr @ serde_json::Value::Array(_) => {
                        let blocks: Vec<Block> = serde_json::from_value(arr).unwrap_or_default();
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
async fn send_retrying<F>(mut build: F) -> Result<(reqwest::StatusCode, String), AgentError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut attempt = 1;
    loop {
        let resp = build().send().await.map_err(|e| AgentError::Http(e.to_string()))?;
        let status = resp.status();
        // Read the hint before consuming the body.
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(retry::parse_retry_after);
        let text = resp.text().await.map_err(|e| AgentError::Http(e.to_string()))?;

        if status.is_success() || !retry::is_retryable(status.as_u16()) || attempt >= retry::MAX_ATTEMPTS {
            return Ok((status, text));
        }
        let wait = retry::delay_for(attempt + 1, retry_after);
        log::warn!(
            "provider returned {status}; waiting {wait:?} before attempt {} of {}",
            attempt + 1,
            retry::MAX_ATTEMPTS
        );
        tokio::time::sleep(wait).await;
        attempt += 1;
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

/// Build a short human-readable label for the spinner while a tool runs.
/// For Bash we include the first 60 chars of the command so it's clear
/// what's executing; for Read/Write/Edit we show the filename.
fn tool_status_label(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let short: String = cmd.chars().take(60).collect();
            let ellipsis = if cmd.len() > 60 { "…" } else { "" };
            format!("Bash: {short}{ellipsis}")
        }
        "Read" | "Write" | "Edit" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            // Show only the last two components so long paths don't overflow.
            // `file_name()` + `parent().and_then(|p| p.file_name())` gets us
            // both in O(1) without the previous quadruple-collect dance.
            let p = std::path::Path::new(path);
            let short = match (p.file_name(), p.parent().and_then(|par| par.file_name())) {
                (Some(file), Some(parent)) => format!("{}/{}", parent.to_string_lossy(), file.to_string_lossy()),
                (Some(file), None) => file.to_string_lossy().into_owned(),
                _ => path.to_string(),
            };
            format!("{name}: {short}")
        }
        "Delegate" => {
            let target = input.get("target").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Delegate → {target}")
        }
        other => other.to_string(),
    }
}

// --- API wire types --------------------------------------------------------

#[derive(Serialize)]
struct MessagesReq<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    tools: &'a [serde_json::Value],
    messages: &'a [ApiMessage],
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

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
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
}
