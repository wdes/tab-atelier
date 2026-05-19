// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The four primitives the agent loop exposes to the model:
//! `Read`, `Write`, `Edit`, `Bash`. Plan-mode gates the three that
//! have side effects. Each module returns a single string back to
//! the model — error vs. success is signalled by the bool the
//! dispatcher pairs with it.

use std::path::Path;

mod bash;
mod delegate;
mod edit;
mod list_agents;
mod read;
mod write;

/// Run a tool by name. `plan_mode = true` makes Write/Edit/Bash
/// refuse with a polite message asking the model to propose instead.
/// Read remains unrestricted — pure observation is always safe.
pub async fn dispatch(name: &str, input: &serde_json::Value, cwd: &Path, plan_mode: bool) -> Result<String, String> {
    match name {
        "Read" => read::run(input, cwd).await,
        "Write" => {
            if plan_mode {
                return Err("Plan-mode is on. Describe the file you want to create instead of writing it.".to_string());
            }
            write::run(input, cwd).await
        }
        "Edit" => {
            if plan_mode {
                return Err("Plan-mode is on. Describe the edit instead of applying it.".to_string());
            }
            edit::run(input, cwd).await
        }
        "Bash" => {
            if plan_mode {
                return Err("Plan-mode is on. Describe the command instead of running it.".to_string());
            }
            bash::run(input, cwd).await
        }
        // Inter-agent tools — observation always allowed; delegation
        // is reads-and-writes-by-proxy, so plan-mode wouldn't really
        // gate it on this side anyway (the *target* enforces its
        // own plan-mode).
        "ListAgents" => list_agents::run(input, cwd).await,
        "Delegate" => delegate::run(input, cwd).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

/// JSON-schema tool specs the model gets in its `tools` array. The
/// shapes are deliberately a strict subset of Claude Code's official
/// tool surface so the model recognises them from its training.
#[must_use]
pub fn tool_specs() -> Vec<serde_json::Value> {
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
