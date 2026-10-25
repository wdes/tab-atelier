// SPDX-License-Identifier: MPL-2.0

//! The one environment variable that tells a child process an agent is driving it.
//!
//! `AI_AGENT` is the cross-agent convention for this. It is not a Composer
//! invention and not ours: Composer added it in `Util\Platform::isCodingAgent()`
//! (composer/composer#13073) alongside the older per-vendor variables
//! (`CLAUDECODE`, `CURSOR_AGENT`, `GEMINI_CLI`, `CODEX_THREAD_ID`, …), taking the
//! name from <https://github.com/laravel/agent-detector>, which in turn is a port
//! of the language-agnostic spec in `vercel/detect-agent`'s `agents.json`. That
//! spec is explicit about the semantics: the variable's trimmed value, when
//! non-empty, *takes highest priority and is emitted verbatim as the agent name*.
//! Laravel's detector does the same, with one special case — a value starting
//! `claude-code` means Claude, anything else is a custom agent named by the value
//! itself.
//!
//! So this is a name, not a flag: `AI_AGENT=catbus` is how a program that asks
//! "was I run by a human or by an agent?" is told `catbus`.
//!
//! ## Why the tools set it rather than the agent
//!
//! Setting it on this process would be simplest, and it is what most agents do —
//! but `std::env::set_var` is `unsafe` under edition 2024, and this crate denies
//! `unsafe` outright (see `logging.rs`). Stamping each command as it is built
//! keeps the guarantee at the spawn site, where it is also testable by running a
//! command and reading what it received.
//!
//! ## Why it is set explicitly rather than inherited
//!
//! `Command::env` replaces as well as adds, so a session that already carried
//! `AI_AGENT` from whatever launched this agent gets our name, not that one.
//! That is the case that matters in practice: a tab opened from inside another
//! agent inherits that agent's variable, and a child that passed it on would
//! report itself as Claude Code while running under catbus.

/// What this agent calls itself in `AI_AGENT`.
///
/// The bare name, following the convention's own examples (`cursor-agent`,
/// `opencode`, `kimi`): the spec emits the value verbatim as the agent's name, so
/// a version suffix would become part of the name rather than metadata about it.
pub const AGENT_NAME: &str = "catbus";

/// Mark a command as agent-driven. Call before spawning.
///
/// Takes `&mut Command` rather than returning one so it composes with the builder
/// chains already at each spawn site, including the ones that add their own
/// environment (`CATBUS_SPAWN_DEPTH`) or resolve their program at runtime.
pub fn mark(cmd: &mut tokio::process::Command) {
    cmd.env("AI_AGENT", AGENT_NAME);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable arrives in the child, which is the only thing that matters.
    ///
    /// Asserted by running a command rather than by inspecting the builder: a
    /// `Command`'s pending environment is observable, but a test that read it
    /// would pass even if the spawn path dropped it.
    #[tokio::test]
    async fn a_marked_command_reports_the_agent_to_its_child() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(r#"printf %s "$AI_AGENT""#);
        mark(&mut cmd);
        let out = cmd.output().await.expect("bash runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), AGENT_NAME);
    }

    /// An inherited name is replaced, not passed on.
    ///
    /// This is the tab-inside-another-agent case: without the replacement the
    /// child would claim to be whatever launched us.
    #[tokio::test]
    async fn an_inherited_name_is_replaced() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(r#"printf %s "$AI_AGENT""#)
            .env("AI_AGENT", "claude-code_2-1-302_agent");
        mark(&mut cmd);
        let out = cmd.output().await.expect("bash runs");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            AGENT_NAME,
            "the child must not inherit another agent's name"
        );
    }
}
