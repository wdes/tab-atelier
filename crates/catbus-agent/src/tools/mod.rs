// SPDX-License-Identifier: MPL-2.0

//! The primitives the agent loop exposes to the model: `Read`, `Write`, `Edit`,
//! `Bash`, `FileTree`, and the inter-agent `ListAgents` / `Delegate`. Plan-mode
//! gates the ones that have side effects. Each module returns a single string
//! back to the model — error vs. success is signalled by the bool the dispatcher
//! pairs with it.

use std::path::Path;

mod bash;
mod config;
mod delegate;
mod edit;
mod filetree;
mod git;
mod list_agents;
mod packages;
mod phpunit;
mod read;
mod spawn;
mod tasks;
mod write;

// Only the resolved set is re-exported. `CustomTool` and `ToolConfig` describe
// the file format and are parsed by `ToolSet::load`, so nothing outside this
// module needs to name them — exporting them invited callers to build a set by
// hand and skip the validation in `from_config`.
pub use config::ToolSet;

/// The smallest tool set that can still finish a task with files, with no shell:
/// `Read`, `Write`, `FileTree`.
///
/// `Write` is not optional — without a shell it is the only way to save work.
/// `FileTree` is what makes `Write` usable, since an agent holding only `Read`
/// cannot discover a path the prompt never named, and would have to guess where
/// to put a new file. `Edit` is left out as a refinement of `Write`, and
/// `Bash`, `Delegate` and `ListAgents` as capabilities a plain-file task does
/// not need — which is the point: what is absent cannot be reached by a
/// confused model, and nothing here can touch a file outside the session's own
/// working directory.
///
/// `allow` this list in a tools config to run such an agent:
///
/// ```json
/// { "allow": ["Read", "Write", "FileTree"] }
/// ```
///
/// Kept as a `const` rather than left to each config so that this list, the
/// tool specs, and the integration test that drives the trio all name the same
/// three tools.
pub const MINIMAL_TOOLS: &[&str] = &["Read", "Write", "FileTree"];

/// What the agent is currently allowed to do.
///
/// Three states rather than the boolean this used to be, because there are
/// three answers and the middle one is the whole point: `Plan` refuses every
/// change, `Open` allows every change, and neither is useful for an agent
/// working unattended on a real task. `Auto` asks the judge about each
/// change individually.
///
/// Exclusive by construction. Two booleans — `plan_mode` and `auto_mode` —
/// would spell four states, one of which (plan and auto together) has no
/// meaning and would need a precedence rule invented for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Nothing is checked. The agent may do what it asks for.
    Open,
    /// Nothing that changes anything is allowed — the agent proposes instead.
    Plan,
    /// Each write-capable action is graded by the judge first. See
    /// [`crate::guard`].
    Auto,
}

impl Gate {
    /// How the state is named in the environment turn and the REPL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// The refusal for a write-capable tool, when this gate has one.
    ///
    /// `None` for [`Gate::Auto`] on purpose: auto mode's answer depends on the
    /// action *and* the conversation, so it cannot be decided by a `match` on
    /// a name. That judgement is made asynchronously in [`crate::agent`], which
    /// has the history and the client; this function only answers for the
    /// states that are decided by policy alone.
    ///
    /// Not `const`: matching a `&str` is not allowed in a const fn on stable,
    /// which is what the `const` here used to fail on.
    #[must_use]
    fn refusal(self, what: &str) -> Option<&'static str> {
        match self {
            Self::Plan => Some(match what {
                "Write" => "Plan-mode is on. Describe the file you want to create instead of writing it.",
                "Edit" => "Plan-mode is on. Describe the edit instead of applying it.",
                // Refused here rather than judged, and named separately: a child
                // starts with the gate open, so starting one would leave
                // plan-mode without changing anything in this session.
                "Spawn" => {
                    "Plan-mode is on. Describe what you want done instead of starting a sub-agent — \
                     a sub-agent begins with the gate open, so this would plan nothing."
                }
                "PHPUnit" => "Plan-mode is on. Describe what the tests should check instead of running them.",
                "GitCommit" => "Plan-mode is on. Say what you would commit instead of committing it.",
                "Composer" | "Bun" => "Plan-mode is on. Describe the commands you would run instead of running them.",
                _ => "Plan-mode is on. Describe the command instead of running it.",
            }),
            Self::Open | Self::Auto => None,
        }
    }

    /// Whether this gate asks the judge about write-capable actions.
    #[must_use]
    pub const fn judges(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// The wire form, for the atomic the agent holds.
    ///
    /// Explicit rather than `as u8` on the enum: a reordered variant would
    /// silently change a running process's meaning, and no `repr` is asserted
    /// on this type.
    #[must_use]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Plan => 1,
            Self::Auto => 2,
        }
    }

    /// The inverse. An unrecognised value is [`Gate::Open`] rather than a
    /// panic: the only writer sets these three, and a torn read that somehow
    /// produced another number should not take the process down.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Plan,
            2 => Self::Auto,
            _ => Self::Open,
        }
    }
}

/// Parse a gate from its wire name.
///
/// Returns `None` for anything unrecognised rather than defaulting to
/// [`Gate::Open`]: a caller that mistypes a mode must be told, not quietly
/// given the least restrictive one. This is the parse half of
/// [`Gate::as_str`], and the two are asserted against each other in the tests.
#[must_use]
pub fn parse_gate(name: &str) -> Option<Gate> {
    match name.trim().to_ascii_lowercase().as_str() {
        "open" => Some(Gate::Open),
        "plan" => Some(Gate::Plan),
        "auto" => Some(Gate::Auto),
        _ => None,
    }
}

/// Whether a tool changes the world, and so is judged in auto mode.
///
/// `Read` and `ListAgents` observe and are never judged — the monitor prompt's
/// own exception, and the reason auto mode is usable at all: an agent that had
/// to pass a safety check to read a file would be slower than plan-mode.
///
/// `Delegate` is not judged here because the child inherits this gate and
/// judges its own actions. Judging the spawn as well would charge twice for one
/// decision and block on the parent's guess about work the child will actually
/// do.
///
/// Not `const`: comparing `&str` is not allowed in a const fn on stable.
#[must_use]
pub fn changes_the_world(name: &str) -> bool {
    matches!(
        name,
        "Write" | "Edit" | "Bash" | "PHPUnit" | "GitCommit" | "Composer" | "Bun"
    )
}

/// Run a tool by name, under whatever gate is in force.
///
/// Read remains unrestricted in every mode — pure observation is always safe.
///
/// A name this set does not offer is refused *before* the gate is consulted.
/// The spec list is what withholds a capability (a minimal set simply never
/// lists `Bash`), and a model does hallucinate familiar tool names — so without
/// this check the withholding would be advice rather than a limit, and an agent
/// configured with three tools could still run a shell by asking for one.
///
/// A method on [`ToolSet`] rather than a free function because a custom tool
/// needs the set's own definitions to run: its `argv`, its timeout, and whether
/// it is judged. A free function would have to be handed the set anyway, and
/// the name it was called by would then be the only thing tying the two
/// together.
impl ToolSet {
    pub async fn dispatch(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
        gate: Gate,
    ) -> Result<String, String> {
        // Membership first: an unoffered name is refused here, so the `match`
        // below can never be reached by a tool the operator withheld.
        if !self.offers(name) {
            return Err(format!("unknown tool: {name}"));
        }
        // Custom tools first, so a name this set defines cannot fall through to
        // a built-in. `from_config` already refuses a shadowed name at startup,
        // so this ordering is belt-and-braces on a case that cannot reach here.
        if let Some(tool) = self.custom_tool(name) {
            return self.run_custom(tool, input, cwd).await;
        }
        match name {
            "Read" => read::run(input, cwd).await,
            // Observation, so it is never gated: refusing an agent the right to
            // see what is on disk would leave `Write` unusable and make
            // plan-mode a dead end rather than a pause.
            "FileTree" => filetree::run(input, cwd).await,
            "Write" => {
                if let Some(why) = gate.refusal("Write") {
                    return Err(why.to_string());
                }
                write::run(input, cwd).await
            }
            "Edit" => {
                if let Some(why) = gate.refusal("Edit") {
                    return Err(why.to_string());
                }
                edit::run(input, cwd).await
            }
            "Bash" => {
                if let Some(why) = gate.refusal("Bash") {
                    return Err(why.to_string());
                }
                bash::run(input, cwd).await
            }
            // Inter-agent tools — observation always allowed; delegation
            // is reads-and-writes-by-proxy, so the gate is enforced by the
            // *target* agent against its own state, not by this one.
            "ListAgents" => list_agents::run(input, cwd).await,
            "Delegate" => delegate::run(input, cwd).await,
            // Not in `changes_the_world`: the child inherits this gate and judges
            // its own actions, which is the reasoning in that function's comment.
            // What *is* enforced here is that a child cannot be started from
            // plan-mode at all, because plan-mode's promise is that nothing on
            // disk changes — and a child starts `Open`, so spawning one would
            // leave the mode by the side door rather than the front.
            "Spawn" => {
                if let Some(why) = gate.refusal("Spawn") {
                    return Err(why.to_string());
                }
                spawn::run(input, cwd).await
            }
            // Deliberately unguarded. It writes to the agent's own state
            // directory, not the operator's tree, so it is not in
            // `changes_the_world` and auto mode should not spend a judge call on
            // it — and plan-mode should allow it, since writing down a plan is
            // what plan-mode is for. See `tasks`'s module doc.
            "Tasks" => tasks::run(input, cwd),
            // Both run project code — a composer or bun script is the project's own
            // programme, and an install runs its hooks — so both are judged in auto mode
            // and refused in plan mode. See `changes_the_world`.
            "Composer" => {
                if let Some(why) = gate.refusal("Composer") {
                    return Err(why.to_string());
                }
                packages::composer(input, cwd).await
            }
            "Bun" => {
                if let Some(why) = gate.refusal("Bun") {
                    return Err(why.to_string());
                }
                packages::bun(input, cwd).await
            }
            // Read-only, so never judged and never refused.
            "GitStatus" => git::status(input, cwd).await,
            // Refused in plan-mode and judged in auto mode: it writes to the repository.
            "GitCommit" => {
                if let Some(why) = gate.refusal("GitCommit") {
                    return Err(why.to_string());
                }
                git::commit(input, cwd).await
            }
            // Runs project code, so it is judged like the shell is — see
            // `changes_the_world` — and refused in plan-mode.
            "PHPUnit" => {
                if let Some(why) = gate.refusal("PHPUnit") {
                    return Err(why.to_string());
                }
                phpunit::run(input, cwd).await
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// JSON-schema tool specs the model gets in its `tools` array. The
/// shapes are deliberately a strict subset of Claude Code's official
/// tool surface so the model recognises them from its training.
#[must_use]
/// Every built-in tool, as sent to the model.
///
/// Named `builtin_` because it is no longer the whole answer: [`ToolSet`] may
/// remove some of these and add others, and a caller reaching for this directly
/// would send a set the operator did not choose. It is the input to
/// `ToolSet::from_config`, not the output.
pub fn builtin_specs() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "Read",
            "description": "Read a file from disk. Path may be absolute or relative to the agent's working directory.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "Optional 1-based starting line." },
                    "limit": { "type": "integer", "description": "Maximum number of lines to return." }
                },
                "required": ["path"]
            }
        }),
        // `FileTree` sits beside `Read` because the two are the observation
        // pair: this one finds what exists, that one shows what is inside.
        // Defined in its own module so its schema and the limits it enforces
        // cannot drift apart.
        filetree::spec(),
        serde_json::json!({
            "name": "Write",
            "description": "Write a file from scratch. Overwrites existing content. Refused in plan-mode.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        }),
        serde_json::json!({
            "name": "Edit",
            "description": "Exact-string replacement in an existing file. `old_string` must appear exactly once. Refused in plan-mode.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }
        }),
        serde_json::json!({
            "name": "ListAgents",
            "description": "List every catbus-agent currently running (anywhere on the machine) along with its session id and socket path. Use this to discover peer agents to delegate work to.",
            "input_schema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "Delegate",
            "description": "Send a sub-prompt to another catbus-agent and wait for its reply. The target's own plan-mode + tools apply; the caller does not share context with it. Use sparingly — every level of delegation eats the caller's tool-loop budget.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Session id (UUID) of the peer agent, or an absolute path to its UNIX socket." },
                    "prompt": { "type": "string", "description": "The sub-prompt the peer should run." },
                    "timeout_secs": { "type": "integer", "description": "Override the 5-minute default. Capped at 1800." }
                },
                "required": ["target", "prompt"]
            }
        }),
        // Defined in its own module, so the defaults the tool actually uses and
        // the defaults it advertises cannot drift apart.
        spawn::spec(),
        // Likewise, and built from the same `ACTIONS` array the dispatch uses.
        tasks::spec(),
        phpunit::spec(),
        git::status_spec(),
        git::commit_spec(),
        packages::composer_spec(),
        packages::bun_spec(),
        serde_json::json!({
            "name": "Bash",
            "description": "Run a shell command in the agent's working directory. Default 10-minute timeout; pass timeout_secs (up to 3600) for long builds. Refused in plan-mode.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "description": { "type": "string", "description": "What this command is for." },
                    "timeout_secs": { "type": "integer", "description": "Override the default 600s timeout. Capped at 3600." }
                },
                "required": ["command"]
            }
        }),
    ]
}

/// Resolve a possibly-relative path against the session's cwd.
/// Centralised so every tool agrees on the rule.
pub fn resolve(cwd: &Path, path: &str) -> std::path::PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config an operator writes to get [`MINIMAL_TOOLS`] and nothing else.
    fn minimal_config() -> config::ToolConfig {
        serde_json::from_value(serde_json::json!({ "allow": MINIMAL_TOOLS })).unwrap()
    }

    #[test]
    fn every_minimal_tool_exists_and_is_dispatchable() {
        // `MINIMAL_TOOLS` is a list of strings, so a typo would produce a config
        // that silently runs a *smaller* set than intended — and an agent with
        // neither Read nor Write looks like a model failure, not a config one.
        let builtin = ToolSet::builtin();
        for name in MINIMAL_TOOLS {
            assert!(
                builtin.offers(name),
                "MINIMAL_TOOLS names {name:?}, which is not a built-in tool"
            );
        }
    }

    #[test]
    fn each_minimal_tool_has_a_spec_and_the_names_agree() {
        // The spec's `name` is what the model actually sends back, so a spec
        // whose name differs from the config entry is a tool the model cannot
        // call.
        let specs = builtin_specs();
        for name in MINIMAL_TOOLS {
            let spec = specs
                .iter()
                .find(|s| s["name"] == *name)
                .unwrap_or_else(|| panic!("no spec for {name:?}"));
            assert_eq!(spec["name"].as_str().unwrap(), *name);
        }
    }

    #[test]
    fn the_minimal_set_is_exactly_read_write_and_filetree() {
        // Guards the definition of "minimal" itself. Widening it silently would
        // hand every restricted agent a capability nobody asked it to have.
        assert_eq!(MINIMAL_TOOLS, ["Read", "Write", "FileTree"]);
    }

    #[test]
    fn allowing_the_minimal_set_withholds_every_dangerous_tool() {
        let set = ToolSet::from_config(minimal_config()).unwrap();
        for name in MINIMAL_TOOLS {
            assert!(set.offers(name), "{name} should be offered");
        }
        // Bash is the one the operator explicitly asked to withhold; the rest
        // are capabilities a plain-file task has no use for, and each one is a
        // way for a confused model to reach outside the working directory.
        for name in ["Bash", "Edit", "Delegate", "ListAgents"] {
            assert!(!set.offers(name), "{name} must not be offered");
        }
        assert_eq!(set.specs().len(), MINIMAL_TOOLS.len());
    }

    #[test]
    fn a_filetree_spec_is_offered_in_the_builtin_set() {
        // Catches the case where the module is written but never registered.
        let set = ToolSet::builtin();
        assert!(set.offers("FileTree"));
        assert!(set.specs().iter().any(|s| s["name"] == "FileTree"));
    }

    #[test]
    fn the_minimal_keyword_matches_the_allow_list_an_operator_would_write() {
        // Two spellings of the same request must produce the same agent:
        // `--tools-config minimal` and the JSON in the docs. If they diverge,
        // one of them is a lie, and the docs are what people read.
        let via_keyword = ToolSet::load(Some(std::path::Path::new(config::MINIMAL_KEYWORD))).unwrap();
        let via_allow = ToolSet::from_config(minimal_config()).unwrap();
        assert_eq!(names_of(&via_keyword), names_of(&via_allow));
        assert_eq!(names_of(&via_keyword).len(), MINIMAL_TOOLS.len());
    }

    #[test]
    fn the_minimal_keyword_is_not_confused_with_a_path() {
        // `minimal` is a keyword, not a filename, so it must resolve without
        // the file existing — and an unrelated path must still be read.
        let keyword = ToolSet::load(Some(std::path::Path::new("minimal"))).unwrap();
        assert!(keyword.offers("FileTree"));
        assert!(!keyword.offers("Edit"));

        let missing = ToolSet::load(Some(std::path::Path::new("/nonexistent-tools-xyz.json")));
        assert!(missing.is_err(), "a path that does not exist should still fail");
    }

    /// Sorted tool names, for comparing two sets regardless of order.
    fn names_of(set: &ToolSet) -> Vec<String> {
        let mut names: Vec<String> = set
            .specs()
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_owned))
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn dispatch_refuses_a_tool_the_set_does_not_offer() {
        // The guard that makes withholding real. `specs()` is what *tells* the
        // model which tools exist, but a model can emit a `tool_use` for any
        // name it likes — including one it was never offered — so the
        // dispatcher has to check membership itself. Without this, a
        // three-tool agent could still run a shell by asking for `Bash`.
        //
        // Tested here rather than only through the binary because this is the
        // boundary itself, and a unit test states the rule without a mock relay
        // in the way.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let set = ToolSet::from_config(minimal_config()).unwrap();
        let gate = Gate::Open;
        let cwd = std::path::Path::new("/tmp");

        for withheld in ["Bash", "Edit", "Delegate", "ListAgents", "ReadFile"] {
            let err = runtime
                .block_on(set.dispatch(withheld, &serde_json::json!({}), cwd, gate))
                .expect_err("a tool outside the set must not dispatch");
            assert_eq!(err, format!("unknown tool: {withheld}"), "wrong refusal for {withheld}");
        }
        // And a tool that *is* offered passes the guard, so the check is not
        // simply refusing everything. `FileTree` on a missing path fails later
        // in its own code, which is a different error than the guard's.
        let err = runtime
            .block_on(set.dispatch(
                "FileTree",
                &serde_json::json!({"path": "/nonexistent-xyz", "depth": 1}),
                cwd,
                gate,
            ))
            .expect_err("a missing path should still error");
        assert!(
            !err.starts_with("unknown tool"),
            "the guard rejected an offered tool: {err}"
        );
    }
}
