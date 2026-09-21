// SPDX-License-Identifier: MPL-2.0

//! Catbus — the agent that drives one Claude session per process.
//!
//! Named after the many-windowed feline conveyance from *My Neighbor
//! Totoro*. Each `tab-atelier` tab can run one catbus instance, and
//! you talk to it through a per-session UNIX socket. Internally it
//! talks to a tab-atelier relay, which holds the Claude subscription
//! login and forwards to Anthropic — so a Max subscription works
//! without an API key, and without the login ever reaching this
//! process. It can also talk to any OpenAI-compatible service instead
//! (`--openai-url`, `--openai-token` and `--openai-model` for
//! `x.ai`/Grok, `OpenAI`, a local server, etc., or the
//! `--infomaniak-*` shortcut for Infomaniak AI Tools).
//! It persists the conversation in the same JSONL shape Claude Code
//! uses (so the existing `/tabs/N/catbus/messages` endpoint Just
//! Works), and runs a small Read / Write / Edit / Bash tool loop.

#![allow(clippy::module_name_repetitions)]

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

mod agent;
mod ansi;
mod applink;
mod cache;
mod cost;
mod guard;
mod identity;
mod logging;
mod openai;
mod relay;
mod retry;
mod session;
mod slash;
mod socket;
mod statusline;
mod tools;
mod tui;

// A clap `Args` struct is a bag of independent flags: each one is genuinely
// boolean and unrelated to the others, which is the shape this lint exists to
// discourage in a *domain* type. Modelling them as one enum would fuse flags
// that can be combined (`--print-socket --once`), so the allow is scoped to this
// struct rather than the crate. The root crate makes the same call for `AppState`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug)]
#[command(
    version,
    about = "Claude agent for tab-atelier. Many tabs, many windows.",
    long_about = None,
)]
struct Args {
    /// Working directory the session runs in. Defaults to $PWD.
    /// Mirrors Claude Code's `cwd` field on every message.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Resume an existing session by id. Without it a **new** session is started: continuing an
    /// earlier conversation is always explicit, so reopening a tab does not silently carry on
    /// whichever one was last in this directory, and two agents here cannot share a history.
    #[arg(long)]
    resume: Option<String>,

    /// Accepted and does nothing: a new session is the default.
    ///
    /// Kept because callers pass it — the app's tab launcher and the `Spawn` tool both name a fresh
    /// session explicitly — and a flag that vanished would be a break for them. `--resume` still
    /// wins over it when both are given.
    #[arg(long)]
    new_session: bool,

    /// Set or update the human-readable name for this session.
    #[arg(long)]
    name: Option<String>,

    /// Path to the UNIX socket the agent listens on. Defaults to
    /// `~/.claude/projects/{escaped-cwd}/{session-id}.sock`, so any
    /// external client that knows the session id can connect.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Print the resolved socket path and exit. Handy for the
    /// tab-atelier API server when it needs to forward POSTs.
    #[arg(long)]
    print_socket: bool,

    /// Skip the in-tab REPL and only listen on the UNIX socket.
    /// Useful when catbus-agent is launched as a background service
    /// rather than from a tab the user is staring at.
    #[arg(long)]
    no_tui: bool,

    /// Answer one socket prompt, then exit.
    ///
    /// This is how a sub-agent started by the `Spawn` tool is run. It is not a
    /// convenience: a child that stays up until its parent reaps it cannot
    /// survive a parent that dies first, and a parent that is killed mid-call
    /// runs no cleanup at all. A process that leaves on its own cannot be
    /// orphaned.
    #[arg(long)]
    once: bool,

    /// Start in this permission mode: `open` (everything allowed, the default),
    /// `auto` (a judge checks each write/edit/bash first), or `plan` (write,
    /// edit, bash and spawn propose instead of acting).
    ///
    /// Overrides the mode the session was last left in, so a launcher can pin it
    /// for a tab. Without this there was no way to start in a mode: the gate was
    /// reachable only through a slash command or the socket, and neither survives
    /// the agent being restarted, which is what a tab reopen does.
    #[arg(long, value_name = "MODE")]
    gate: Option<String>,

    /// Use this text as the whole system prompt, instead of the built-in one.
    ///
    /// The built-in prompt tells the model it is Claude Code, which is untrue on
    /// any endpoint that is not Anthropic's, and the terminal-rendering rules are
    /// still appended after this. Takes precedence over `--identity-file`.
    #[arg(long, value_name = "TEXT", env = "CATBUS_IDENTITY")]
    identity: Option<String>,

    /// Read the system prompt from this file — markdown, optionally with
    /// `---`-delimited front matter that may set `AllowedTools`.
    ///
    /// The text replaces the whole system prompt. A blank file means "send no
    /// identity at all". Naming a file that cannot be read, or that holds no
    /// prompt, is an error: it was asked for by name, so a typo should be loud.
    /// Without this flag (and without `CATBUS_IDENTITY_FILE`) the built-in
    /// location is used when it happens to exist, where absence means the
    /// built-in prompt rather than an error.
    #[arg(long, value_name = "PATH", env = "CATBUS_IDENTITY_FILE")]
    identity_file: Option<PathBuf>,

    /// Allow ANSI escapes in text replies.
    ///
    /// With no flag, escapes are allowed only when stdout is a terminal *and*
    /// the environment has not asked for no colour — a session read over the
    /// socket, shown on a phone, or marked `NO_COLOR` by tab-atelier gets
    /// plain prose instead, because `[1m` is what a reader with no terminal
    /// sees otherwise. Setting this forces escapes on, and `--ansi=false`
    /// forces them off.
    ///
    /// Escape sequences are filtered out of anything a non-terminal reader
    /// would see regardless, so a model that ignores the instruction cannot
    /// leak them into the transcript.
    ///
    /// `num_args = 0..=1` is what lets it be written bare (`--ansi`) while
    /// still accepting an explicit `--ansi=false`. A plain `Option<bool>`
    /// would require a value, and the bare form is the one people type.
    #[arg(long, env = "CATBUS_ANSI", num_args = 0..=1, default_missing_value = "true")]
    ansi: Option<bool>,

    /// Relay to talk to, e.g. `https://proxy.example` or the full
    /// `https://proxy.example/relay/anthropic`. Defaults to the relay
    /// endpoint in tab-atelier's preferences.json, so a machine that
    /// already runs tab-atelier needs no flag.
    #[arg(long, env = "CATBUS_RELAY_URL")]
    relay_url: Option<String>,

    /// This machine's relay token, minted on the proxy. Sent as
    /// `x-api-key`. Prefer the env var over the flag so the secret stays
    /// out of `ps` output and shell history.
    #[arg(long, env = "CATBUS_RELAY_TOKEN", hide_env_values = true)]
    relay_token: Option<String>,

    /// Model auto mode grades actions with. Defaults to a cheap
    /// Flash-class model rather than this session's own model: judging a
    /// proposed command is classification, not reasoning, and grading every
    /// write with a heavy model costs an order of magnitude more than the
    /// judgement is worth. The judge's prompt is fixed, so it caches after the
    /// first call either way.
    #[arg(long, env = "CATBUS_JUDGE_MODEL")]
    judge_model: Option<String>,

    /// File holding the monitor prompt for auto mode, in place of the
    /// built-in one. This exists so an operator can install the exact prompt
    /// their provider uses without this repository carrying it.
    #[arg(long, env = "CATBUS_MONITOR_PROMPT")]
    monitor_prompt: Option<PathBuf>,

    /// JSON file configuring the tool set: `{"disable": [...], "allow": [...],
    /// "add": [...]}`. Resolved once at startup, so changing it takes a
    /// restart — a tool array that moved mid-session would invalidate the
    /// prompt cache on every turn.
    ///
    /// The literal word `minimal` is accepted instead of a path, as a
    /// shorthand for `{"allow": ["Read", "Write", "FileTree", "Grep"]}` — a file-editing
    /// agent with no shell. A name that is neither a path nor `minimal` is an
    /// error rather than a silent fallback to the full set.
    #[arg(long, env = "CATBUS_TOOLS_CONFIG")]
    tools_config: Option<PathBuf>,

    /// Base URL of any OpenAI-compatible service, e.g.
    /// `https://api.x.ai/v1` (Grok) or `http://localhost:11434/v1`
    /// (Ollama). `/chat/completions` is appended when missing. Routes
    /// the session through that service instead of the relay.
    ///
    /// Deliberately *not* `conflicts_with` the relay flags: a machine
    /// that relays by default exports `CATBUS_RELAY_URL`, and that must
    /// not stop someone reaching for a different backend. An
    /// OpenAI-compatible backend simply takes precedence when both are
    /// configured.
    #[arg(
        long,
        env = "CATBUS_OPENAI_URL",
        requires = "openai_token",
        requires = "openai_model",
        conflicts_with = "infomaniak_product_id"
    )]
    openai_url: Option<String>,

    /// API token for --openai-url (sent as a Bearer header). Prefer
    /// the env var over the flag so the secret stays out of `ps`
    /// output and shell history.
    #[arg(long, env = "CATBUS_OPENAI_TOKEN", hide_env_values = true, requires = "openai_url")]
    openai_token: Option<String>,

    /// Model to request from --openai-url (e.g. grok-4). Pick a
    /// function-calling-capable model or the tool loop degrades to
    /// text-only answers.
    #[arg(long, env = "CATBUS_OPENAI_MODEL", requires = "openai_url")]
    openai_model: Option<String>,

    /// Infomaniak AI Tools product id — a shortcut for --openai-url
    /// that builds the product-scoped Infomaniak endpoint. Together
    /// with --infomaniak-token this routes the session through
    /// Infomaniak's OpenAI-compatible API instead of the relay.
    #[arg(long, env = "INFOMANIAK_PRODUCT_ID", requires = "infomaniak_token")]
    infomaniak_product_id: Option<String>,

    /// Infomaniak API token (sent as a Bearer header). Prefer the env
    /// var over the flag so the secret stays out of `ps` output and
    /// shell history.
    #[arg(
        long,
        env = "INFOMANIAK_API_TOKEN",
        hide_env_values = true,
        requires = "infomaniak_product_id"
    )]
    infomaniak_token: Option<String>,

    /// Model to request from Infomaniak. Only used with the two flags
    /// above; pick a function-calling-capable model or the tool loop
    /// degrades to text-only answers.
    #[arg(long, env = "INFOMANIAK_MODEL", default_value = openai::INFOMANIAK_DEFAULT_MODEL)]
    infomaniak_model: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Print the error's Display form. Returning a Result from `main`
    // would print the Debug form instead, which for the relay errors
    // buries the one sentence telling the operator what to configure
    // inside an enum dump.
    if let Err(e) = run().await {
        eprintln!("catbus-agent: {e}");
        std::process::exit(1);
    }
}

/// Resolve the operator's prompt file and the tool set they end up with.
///
/// Both are resolved together, and before the agent exists, because the prompt
/// file is one of the two sources the tool set comes from. A bad file or a bad
/// tool name is a startup failure rather than something that surfaces as a failed
/// turn later.
///
/// `AllowedTools` in the front matter **narrows** the set the launcher configured
/// and never widens it: `narrowed_to` refuses a name the launcher's `--tools-config`
/// withheld, so a prompt file cannot grant itself a tool. That is the whole point
/// of the ceiling — the wrapper answer is the same tool list the operator already
/// chose, minus what the prompt file removes.
fn resolve_tools(args: &Args) -> Result<(tools::ToolSet, identity::Identity), Box<dyn std::error::Error>> {
    let tool_set = tools::ToolSet::load(args.tools_config.as_deref())?;
    let identity = identity::load(args.identity.as_deref(), args.identity_file.as_deref())?;
    let Some(allowed) = identity.allowed_tools() else {
        log::info!("offering {} tools", tool_set.specs().len());
        return Ok((tool_set, identity));
    };

    let narrowed = tool_set.narrowed_to(allowed)?;
    log::info!(
        "the identity file limits the tool set to {} (from {})",
        narrowed.names().join(", "),
        tool_set.names().join(", ")
    );
    Ok((narrowed, identity))
}

/// Apply `--gate`, if the launcher gave one.
///
/// An explicit mode wins over the one the session was last left in. Applied here rather than in
/// `Agent::new` because the flag belongs to this launch while the saved mode belongs to the session —
/// and it is written back, so pinning a tab once is enough. An unknown word is a hard error: it was
/// typed by a launcher, so it is a mistake to fix rather than something to default away.
async fn apply_launch_gate(agent: &Arc<agent::Agent>, word: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(word) = word else {
        return Ok(());
    };
    let gate = tools::parse_gate(word).ok_or_else(|| format!("unknown --gate `{word}` — one of: open, auto, plan"))?;
    agent.set_gate(gate).await;
    Ok(())
}

/// Ask the relay what it charges, without making the session wait for the answer.
///
/// In the background on purpose. A price list is an enhancement, and blocking on one before the
/// session can be typed into is the wrong trade: against an endpoint that black-holes rather than
/// refusing — a firewall, a typo'd host — the wait is the client's whole connect timeout, and the
/// operator stares at a session that has not started for reasons nothing on screen explains. Found
/// by a test that types at a REPL whose relay is a dead port: the prompt simply never appeared.
///
/// Until it lands, the totals show tokens with no amounts — the same state as a relay that serves
/// no prices at all — so nothing waits on it and nothing depends on it succeeding. It is logged at
/// `info`, because a relay without one is not a fault.
/// Tell the app this tab now has an agent, before the first turn.
///
/// The id is the point: it is what the app stores and hands back through `--resume`, so without this a
/// reopened tab would start a blank session and the conversation would look lost. `waiting`, not
/// `thinking` — an agent sitting at its prompt is waiting for the operator.
///
/// Spawned like every report but the exit one, so start-up does not wait on a request to the app.
fn announce_to_app(agent: &Arc<agent::Agent>) {
    let Some(endpoint) = applink::endpoint() else {
        return;
    };
    let session = agent.session_id_for_report();
    tokio::spawn(async move {
        applink::report(&endpoint, applink::State::Waiting, None, &session).await;
    });
}

fn fetch_prices_in_background(agent: &Arc<agent::Agent>) {
    let agent = Arc::clone(agent);
    tokio::spawn(async move {
        if let Err(why) = agent.fetch_prices().await {
            log::info!("no prices available: {why}");
        }
    });
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Destination and level floor are decided together, because the floor depends on the
    // destination — a log sharing the terminal with the TUI has to be quiet. See `logging`.
    logging::init(args.no_tui);

    // Resolved early, because the code below moves fields out of `args` — the cwd
    // and the provider's own config — and these two need to read it. The logger is
    // initialised first so that anything they report is actually seen.
    let (tool_set, identity) = resolve_tools(&args)?;

    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    // The relay must resolve *before* we open the socket — no point
    // accepting prompts we can't service. Relay resolution only reads
    // this machine's own preferences and token; the subscription login
    // stays on the proxy, so catbus works on a box with no Claude Code
    // credentials at all.
    let provider =
        if let (Some(url), Some(token), Some(model)) = (args.openai_url, args.openai_token, args.openai_model) {
            agent::Provider::OpenAiCompat(openai::Config {
                chat_url: openai::chat_url_from_base(&url),
                token,
                model,
            })
        } else if let (Some(product_id), Some(token)) = (args.infomaniak_product_id, args.infomaniak_token) {
            agent::Provider::OpenAiCompat(openai::Config {
                chat_url: openai::infomaniak_chat_url(&product_id),
                token,
                model: args.infomaniak_model,
            })
        } else {
            let relay = relay::Relay::resolve(args.relay_url.as_deref(), args.relay_token.as_deref())?;
            log::info!("relaying to {}", relay.base_url());
            agent::Provider::Relay(relay)
        };
    let session = session::open(&cwd, args.resume.as_deref(), args.new_session)?;

    // Apply --name if provided (also works as a rename on resume).
    if let Some(ref name) = args.name {
        session.rename(name)?;
    }

    let socket_path = args.socket.clone().unwrap_or_else(|| session.default_socket_path());

    if args.print_socket {
        println!("{}", socket_path.display());
        return Ok(());
    }

    // Refuse to start a second agent on a session that already has one.
    //
    // Two agents on one session is not a supported shape, and it fails in a way
    // that names nothing useful. The default socket is derived from the session
    // id, so the second agent binds the first one's socket: tab-atelier then
    // talks to whichever agent won, while the other keeps its own unrelated
    // history. Both append to the same transcript, so the file alternates
    // between two conversations — and since each holds an in-memory history that
    // its sibling keeps invalidating, the transcript can end up with turns no
    // single agent's history matches. The symptom is a 400 about a message
    // number that does not correspond to anything the operator typed.
    //
    // `--socket` means an operator (or the `Spawn` tool) has picked the endpoint
    // deliberately and may be starting a second agent for the same directory on
    // purpose, so the guard is for the derived path: "do not take over a session
    // that is already being served".
    if args.socket.is_none() && socket::is_live(&socket_path) {
        let id = &session.id;
        let short = id.get(..8).unwrap_or(id);
        return Err(format!(
            "a catbus-agent is already serving session {short} at {}.\n  \
             Another agent here would share this session's transcript with it, and neither \
             would have a consistent history.\n  \
             Use that agent — or, to start a second one, pass --socket <path> so the two \
             do not collide.",
            socket_path.display()
        )
        .into());
    }

    log::info!("session {} ready at {}", session.id, socket_path.display());

    // Read the monitor prompt before building the agent, so a bad path is a
    // startup failure rather than a surprise the first time auto mode blocks
    // something. The prompt is only used in auto mode, but a setting that is
    // wrong should say so immediately.
    let monitor_prompt = guard::Judge::prompt_from_path(args.monitor_prompt.as_deref())?;
    let judge_model = args
        .judge_model
        .clone()
        .unwrap_or_else(|| guard::DEFAULT_JUDGE_MODEL.to_owned());
    // Resolved here, once, before the agent exists. A bad config is a startup
    // failure: a tool set that silently differs from what the operator wrote
    // would tell the model it has a tool that behaves otherwise.
    // Whether the answer may carry escape sequences. Most explicit source
    // wins: an explicit `--ansi`, then the colour convention, then whether
    // stdout is really a terminal. The middle step is what makes an agent tab
    // come out plain without the launcher saying anything — tab-atelier's
    // `new_tab_env` sets `NO_COLOR=1` for the tabs an *agent* asked for, since
    // those tabs' output is read by another program (`peek`, `output`, a
    // `--wait` poll) and escapes there are bytes nothing renders — and what
    // makes a tab with its right-click colours switched off come out plain too,
    // since that toggle is expressed as `TERM=dumb`. See `ansi::allow_escapes`
    // for the full reasoning.
    let stdout_renders = !args.no_tui && std::io::stdout().is_terminal();
    let env_disables_colour = ansi::colour_disabled_in_env();
    let ansi = ansi::allow_escapes(args.ansi, stdout_renders, env_disables_colour);
    // Log every input, not just the verdict: when the answer looks wrong, the
    // useful question is *which* source decided it. TERM is included because it
    // is the signal the app's per-tab colours toggle uses, and a `TERM=dumb` tab
    // is otherwise indistinguishable from a misconfiguration.
    log::info!(
        "ansi escapes in replies: {ansi} \
         (flag={:?}, stdout_renders={stdout_renders}, colour_disabled={env_disables_colour}, \
         TERM={:?}, NO_COLOR={:?})",
        args.ansi,
        std::env::var("TERM").ok(),
        std::env::var("NO_COLOR").ok()
    );
    let agent = Arc::new(
        agent::Agent::new(provider, session)
            .with_judge(judge_model, monitor_prompt)
            // The identity file's `AllowedHosts`, enforced by the SSH tool itself: the tool needs it
            // mid-call, and the dispatcher is where a tool is reached.
            // The identity file's limits on SSH: which destinations, and which jump hosts. Both
            // travel together, so one cannot be applied while the other is forgotten.
            .with_tools(tool_set.with_ssh_policy(tools::ssh::Policy {
                allowed_hosts: identity.allowed_hosts().map(<[String]>::to_vec),
                allowed_jump_hosts: identity.allowed_jump_hosts().map(<[String]>::to_vec),
            }))
            .with_ansi(ansi)
            .with_identity(identity),
    );

    fetch_prices_in_background(&agent);

    announce_to_app(&agent);

    apply_launch_gate(&agent, args.gate.as_deref()).await?;

    // Stated at start-up, because "which mode am I in" is the first thing an
    // operator needs when a write went through that they expected to be checked,
    // or was refused when they expected it to go. The mode now comes from three
    // places — this flag, the session's saved value, or the default — and this
    // line is the only thing that says which one won.
    log::info!("permission mode: {}", agent.gate().as_str());

    let socket_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        let path = socket_path.clone();
        let once = args.once;
        async move { socket::serve(agent, path, once).await }
    });

    if args.no_tui {
        // Headless: just block on the socket task. With `--once` this returns as
        // soon as the one prompt has been answered, and the process exits.
        socket_task.await??;
    } else {
        tui::app::run(Arc::clone(&agent), &cwd).await?;
        // REPL exit (Ctrl-D) brings the whole process down so the
        // tab the user closed feels "closed". Aborting the socket
        // task removes its file in Drop on a best-effort basis.
        socket_task.abort();
    }
    goodbye_to_app(&agent).await;
    Ok(())
}

/// The last word to the app: this agent is gone, so its tab's indicator should stop rather than keep
/// showing whatever it was last doing.
///
/// Awaited, unlike every other report. The process is about to end, so a spawned task would be
/// cancelled before it sent anything — and this is the one report whose absence leaves a wrong answer
/// on screen rather than a stale one.
async fn goodbye_to_app(agent: &Arc<agent::Agent>) {
    let Some(endpoint) = applink::endpoint() else {
        return;
    };
    applink::report(&endpoint, applink::State::Idle, None, &agent.session_id_for_report()).await;
}

///
/// The gate a bare mode command selects, or `None` if `input` is not one.
///
/// Extracted from the REPL loop because that loop needs a terminal to run at
/// all — reedline puts the tty in raw mode — so a test cannot reach the mapping
/// from the typed word to the mode. That mapping is exactly what can silently
/// break: a command that prints `gate = auto` while setting something else looks
/// identical to a working one.
///
/// Delegates to [`tools::parse_gate`] after stripping the slash rather than
/// keeping its own table. The socket's `set_gate` request parses the same words
/// through the same function, and a second hand-written mapping here is how
/// `/auto` and `{"kind":"set_gate","gate":"auto"}` would come to mean different
/// things — a difference nothing would catch, since both would report
/// `gate = auto`. Delegating makes the two the same code path, and it is why
/// `/auto` can be checked through the socket, where a tty is not required.
///
/// Matched on the whole word, never a prefix: `/autorun` must not enable auto
/// mode, and `/plan the refactor` must stay a prompt.
fn gate_command(input: &str) -> Option<tools::Gate> {
    let word = input.trim().strip_prefix('/')?;
    match word {
        // The two aliases are REPL conveniences with no wire spelling: "no
        // plan" and "no auto" both mean "no gate", and either is what an
        // operator reaches for. Everything else is the shared vocabulary.
        "noplan" | "noauto" => Some(tools::Gate::Open),
        other => tools::parse_gate(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_command_selects_its_own_gate() {
        // The mapping the REPL acts on. `/auto` in particular is the only way to
        // reach the judge, so a silent mismatch here would leave auto mode
        // unreachable while the REPL happily printed "gate = auto".
        assert_eq!(gate_command("/plan"), Some(tools::Gate::Plan));
        assert_eq!(gate_command("/auto"), Some(tools::Gate::Auto));
        assert_eq!(gate_command("/noplan"), Some(tools::Gate::Open));
        assert_eq!(gate_command("/noauto"), Some(tools::Gate::Open));
    }

    #[test]
    fn the_three_modes_are_distinct_and_named_as_the_model_sees_them() {
        // Three words, three states. Two commands mapping to one gate would
        // leave a mode unreachable, and the name the REPL prints has to be the
        // name the agent puts in the environment turn — otherwise the operator
        // reads one thing and the model is told another.
        let gates = [
            gate_command("/plan").unwrap(),
            gate_command("/auto").unwrap(),
            gate_command("/noplan").unwrap(),
        ];
        assert_ne!(gates[0], gates[1]);
        assert_ne!(gates[1], gates[2]);
        assert_ne!(gates[0], gates[2]);
        assert_eq!(tools::Gate::Plan.as_str(), "plan");
        assert_eq!(tools::Gate::Auto.as_str(), "auto");
        assert_eq!(tools::Gate::Open.as_str(), "open");
    }

    #[test]
    fn the_repl_and_the_socket_agree_about_what_each_mode_is_called() {
        // The guarantee that makes `/auto` trustworthy: it resolves through the
        // same function the socket's `set_gate` uses, so the two cannot drift
        // into meaning different things. Without this, the REPL could print
        // `gate = auto` while the socket's spelling of "auto" did something
        // else — and nothing would notice, because both would look right from
        // their own side.
        for gate in [tools::Gate::Open, tools::Gate::Plan, tools::Gate::Auto] {
            let socket_parsed = tools::parse_gate(gate.as_str());
            assert_eq!(
                socket_parsed,
                Some(gate),
                "the wire name {} does not round-trip",
                gate.as_str()
            );
            let repl_parsed = gate_command(&format!("/{}", gate.as_str()));
            assert_eq!(
                repl_parsed,
                socket_parsed,
                "/{} and set_gate({:?}) disagree",
                gate.as_str(),
                gate.as_str()
            );
        }
    }

    #[test]
    fn auto_mode_is_reachable_from_the_repl_word_that_promises_it() {
        // The specific claim behind the command: `/auto` selects the judged
        // mode, not merely a mode that prints "auto".
        assert_eq!(gate_command("/auto"), Some(tools::Gate::Auto));
        assert_eq!(gate_command("/auto").map(tools::Gate::as_str), Some("auto"));
    }

    #[test]
    fn a_command_is_matched_exactly_never_by_prefix() {
        // A prompt is not a command unless it is the whole word. `/autorun` must
        // reach the model as a prompt rather than quietly enabling the judge,
        // and `/plan the refactor` must stay a prompt.
        for not_a_command in [
            "/autorun",
            "/automatic",
            "/plan the refactor",
            "/planning",
            "/noplan/x",
            "plain text",
            "/pla",
            "",
            "  ",
        ] {
            assert_eq!(gate_command(not_a_command), None, "{not_a_command:?} is not a command");
        }
    }

    #[test]
    fn surrounding_whitespace_does_not_hide_a_command() {
        // Reedline hands over the line as typed, and a pasted command can carry
        // a trailing space.
        assert_eq!(gate_command("/auto "), Some(tools::Gate::Auto));
        assert_eq!(gate_command("  /plan"), Some(tools::Gate::Plan));
        // `/clear` is a command, but not a mode: it resolves in `slash`'s table
        // and this parser — which the dispatch reaches only for a `Gate` action —
        // must not claim it. The REPL asks the table first and never gets here
        // for a non-gate, so a match here would be a mode nobody can select.
        assert_eq!(gate_command("/clear "), None, "clear is not a mode");
        assert_eq!(
            slash::lookup("/clear ").map(|(command, _)| command.action),
            Some(slash::Action::Clear),
            "and the table is where it is owned"
        );
    }
}
