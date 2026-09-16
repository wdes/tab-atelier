// SPDX-License-Identifier: MPL-2.0

//! The four primitives the agent loop exposes to the model:
//! `Read`, `Write`, `Edit`, `Bash`. Plan-mode gates the three that
//! have side effects. Each module returns a single string back to
//! the model — error vs. success is signalled by the bool the
//! dispatcher pairs with it.

use std::path::Path;

mod bash;
mod config;
mod delegate;
mod edit;
mod list_agents;
mod read;
mod write;

// Only the resolved set is re-exported. `CustomTool` and `ToolConfig` describe
// the file format and are parsed by `ToolSet::load`, so nothing outside this
// module needs to name them — exporting them invited callers to build a set by
// hand and skip the validation in `from_config`.
pub use config::ToolSet;

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
    matches!(name, "Write" | "Edit" | "Bash")
}

/// Run a tool by name, under whatever gate is in force.
///
/// Read remains unrestricted in every mode — pure observation is always safe.
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
        // Custom tools first, so a name this set defines cannot fall through to
        // a built-in. `from_config` already refuses a shadowed name at startup,
        // so this ordering is belt-and-braces on a case that cannot reach here.
        if let Some(tool) = self.custom_tool(name) {
            return self.run_custom(tool, input, cwd).await;
        }
        match name {
            "Read" => read::run(input, cwd).await,
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
