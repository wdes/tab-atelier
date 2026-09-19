// SPDX-License-Identifier: MPL-2.0

//! What the agent says it is.
//!
//! The first system block used to be Claude Code's own identity line
//! unconditionally, whatever model was actually answering. On a relayed endpoint
//! that belongs to another vendor, so the client asserted something untrue on
//! every turn and the operator had no way to say anything else.
//!
//! This resolves what to say instead, from the operator: a markdown file with
//! optional front matter,
//!
//! ```text
//! ---
//! AllowedTools: Write, Read
//! ---
//! You are a parrot. Answer only in the voice of a parrot.
//! ```
//!
//! The text replaces the whole system prompt, because the operator who writes one
//! owns it. A blank file means "send no identity at all". Nothing supplied means
//! the built-in behaviour, except that the Claude line is dropped once the relay
//! has reported a model that is not Anthropic — see [`Identity::Auto`].

use std::path::{Path, PathBuf};

/// The identity prefix sent when nothing else is configured. Kept here rather
/// than in `agent` so the module that decides *whether* to send it owns the text.
pub const CLAUDE_CODE_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// What to send as the system prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// Nothing configured: the built-in prompt, except that the Claude line is
    /// omitted once a non-Anthropic model has been seen.
    Auto,
    /// An operator-supplied prompt, which replaces the whole system prompt.
    Text {
        text: String,
        /// Tool names the prompt file permits, if it said.
        allowed_tools: Option<Vec<String>>,
    },
    /// Send no identity block at all.
    Omitted,
}

/// Path of the built-in prompt file.
///
/// `$XDG_CONFIG_HOME/tab-atelier/catbus-agent/identity.md`, else under
/// `$HOME/.config`, resolved the way `relay::preferences_path` resolves its own so
/// the two config files sit together.
///
/// Deliberately does **not** consult `CATBUS_IDENTITY_FILE`. That variable names a
/// file explicitly — it is bound to the `--identity-file` flag — and a file named
/// there must exist, which is not true of this one. Letting the same variable mean
/// both "an explicit request" and "a default that may be absent" is how a typo
/// becomes a silently ignored prompt.
#[must_use]
pub fn path() -> PathBuf {
    path_from(non_empty_env("XDG_CONFIG_HOME"), non_empty_env("HOME"))
}

/// [`path`] with the environment passed in.
///
/// Split out because reading the environment in a test needs `set_var`, which
/// edition 2024 makes `unsafe` and this crate denies outright — so the tests
/// exercise this, and the environment only ever enters through [`path`].
fn path_from(xdg_config: Option<String>, home: Option<String>) -> PathBuf {
    let base = xdg_config
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("tab-atelier").join("catbus-agent").join("identity.md")
}

/// Resolve the identity from an inline string and/or an explicit file path.
///
/// Precedence: `inline`, then `file`, then the built-in [`path`]. The line between
/// the last two is the one that matters, and it is about intent rather than
/// existence:
///
/// * A file the operator **named** — `--identity-file`, which is also what
///   `CATBUS_IDENTITY_FILE` sets — must be readable and must hold a prompt. Both
///   failures are errors, because a name that does not resolve is a typo and
///   should be loud rather than quietly ignored.
/// * The **built-in** location is a convenience: absent means [`Identity::Auto`],
///   and present-but-blank means [`Identity::Omitted`], which is how an operator
///   says "send no identity block" without deleting the file's front matter.
pub fn load(inline: Option<&str>, file: Option<&Path>) -> Result<Identity, String> {
    load_at(inline, file, &path())
}

/// [`load`] with the default path passed in, for the same reason as [`path_from`].
fn load_at(inline: Option<&str>, file: Option<&Path>, default: &Path) -> Result<Identity, String> {
    if let Some(text) = inline {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("--identity is empty".to_string());
        }
        // Inline text has no front matter: the whole string is the prompt, so a
        // `---` in it is just a line of the prompt.
        return Ok(Identity::Text {
            text: text.to_owned(),
            allowed_tools: None,
        });
    }

    if let Some(path) = file {
        let raw =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read identity file {}: {e}", path.display()))?;
        let (text, allowed_tools) = parse_prompt(&raw).map_err(|e| format!("identity file {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            return Err(format!(
                "identity file {} has no prompt — remove it, or put the prompt after the front \
                 matter",
                path.display()
            ));
        }
        return Ok(Identity::Text { text, allowed_tools });
    }

    let default = default.to_path_buf();
    let raw = match std::fs::read_to_string(&default) {
        Ok(raw) => raw,
        // Absent is the ordinary case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Identity::Auto),
        // Present but unreadable is not ordinary, and silently falling back would
        // hide a permissions problem behind "the agent ignores my prompt".
        Err(e) => return Err(format!("cannot read identity file {}: {e}", default.display())),
    };
    let (text, allowed_tools) = parse_prompt(&raw).map_err(|e| format!("identity file {}: {e}", default.display()))?;
    if text.trim().is_empty() {
        // A blank file is how the operator says "send nothing".
        return Ok(Identity::Omitted);
    }
    Ok(Identity::Text { text, allowed_tools })
}

/// Split optional front matter from the prompt body.
///
/// Front matter is present only when the first line is exactly `---`, and it runs
/// to the next line that is exactly `---`. That closing line is required: without
/// it the whole file is a header, which is almost certainly not what was meant, so
/// it is an error rather than a prompt that silently begins with dashes.
pub fn parse_prompt(raw: &str) -> Result<(String, Option<Vec<String>>), String> {
    let mut lines = raw.lines();
    let first = lines.next().unwrap_or_default();
    if first.trim_end_matches('\r') != "---" {
        // No front matter: the whole input is the prompt, preserved as written
        // apart from a leading blank line.
        return Ok((raw.trim_start_matches('\n').to_owned(), None));
    }

    let mut allowed_tools: Option<Vec<String>> = None;
    let mut closed = false;
    let mut body_start = 0;
    for (index, line) in lines.by_ref().enumerate() {
        // `+2` because the header line and this one are consumed here.
        let consumed = index + 2;
        if line.trim_end_matches('\r') == "---" {
            closed = true;
            body_start = consumed;
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            // A header line with no colon is ignored rather than rejected: the
            // format is ours to extend, and refusing would make a stray line in a
            // prompt file fatal.
            continue;
        };
        if key.trim().eq_ignore_ascii_case("AllowedTools") {
            allowed_tools = Some(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>(),
            );
        }
    }
    if !closed {
        return Err("front matter opened with --- but never closed with ---".to_string());
    }

    // Rebuilt from the line list rather than sliced, so the body's own use of a
    // `---` line cannot be mistaken for a delimiter.
    let body = raw
        .lines()
        .skip(body_start)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start_matches('\n')
        .to_owned();
    Ok((body, allowed_tools))
}

/// Whether a model name identifies an Anthropic model.
///
/// A case-insensitive `claude` covers the bare id, Bedrock's
/// `anthropic.claude-…` and Vertex's `claude-3-5-sonnet@…`. Anything unrecognised
/// is **not** Anthropic, which is the safe direction: an unknown name means "do
/// not claim to be Claude".
#[must_use]
pub fn is_anthropic(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_front_matter_is_all_prompt() {
        let (text, tools) = parse_prompt("You are a parrot.").unwrap();
        assert_eq!(text, "You are a parrot.");
        assert_eq!(tools, None);
    }

    #[test]
    fn front_matter_yields_the_prompt_after_it() {
        let (text, tools) = parse_prompt("---\nAllowedTools: Write, Read\n---\nYou are a parrot.\n").unwrap();
        assert_eq!(tools.as_deref(), Some(&["Write".to_string(), "Read".to_string()][..]));
        assert_eq!(text, "You are a parrot.");
    }

    #[test]
    fn allowed_tools_tolerates_odd_spacing_and_trailing_commas() {
        let (_, tools) = parse_prompt("---\nAllowedTools:  Write ,Read , , Edit ,\n---\nhi\n").unwrap();
        assert_eq!(
            tools.as_deref(),
            Some(&["Write".to_string(), "Read".to_string(), "Edit".to_string()][..])
        );
    }

    #[test]
    fn an_empty_allowed_tools_line_means_none_named() {
        let (text, tools) = parse_prompt("---\nAllowedTools:\n---\nhi\n").unwrap();
        assert_eq!(tools, Some(Vec::new()));
        assert_eq!(text, "hi");
    }

    /// An unknown key is ignored, not fatal: the format is ours to extend, and a
    /// prompt file must not stop working because a later version wrote a key this
    /// one does not know.
    #[test]
    fn an_unknown_header_key_is_ignored() {
        let (text, tools) = parse_prompt("---\nAuthor: williamdes\nAllowedTools: Read\n---\nhi\n").unwrap();
        assert_eq!(text, "hi");
        assert_eq!(tools.as_deref(), Some(&["Read".to_string()][..]));

        // A stray line with no colon at all, too.
        let (text, _) = parse_prompt("---\nnot a header line\n---\nhi\n").unwrap();
        assert_eq!(text, "hi");
    }

    /// Unterminated front matter is an error, not a prompt that begins with
    /// dashes — the file is wrong and saying so is more useful than guessing.
    #[test]
    fn unterminated_front_matter_is_an_error() {
        let err = parse_prompt("---\nAllowedTools: Read\nYou are a parrot.\n").unwrap_err();
        assert!(err.contains("never closed"), "{err}");
    }

    /// The header is delimited by its own closing line; a `---` in the body is
    /// part of the prompt, which matters because markdown uses them as rules.
    #[test]
    fn a_delimiter_in_the_body_is_not_a_delimiter() {
        let (text, _) = parse_prompt("---\nAllowedTools: Read\n---\npara one\n---\npara two\n").unwrap();
        assert_eq!(text, "para one\n---\npara two");
    }

    /// Front matter with an empty body is an empty prompt, not an error: it is
    /// `load` that decides a blank prompt is "send nothing".
    #[test]
    fn front_matter_with_no_body_gives_an_empty_prompt() {
        let (text, tools) = parse_prompt("---\nAllowedTools: Read\n---\n").unwrap();
        assert_eq!(text, "");
        assert_eq!(tools.as_deref(), Some(&["Read".to_string()][..]));
    }

    #[test]
    fn the_body_keeps_its_own_formatting() {
        // Only leading blank lines are dropped; indentation and inner spacing are
        // the author's.
        let (text, _) = parse_prompt("---\n---\n\n  indented\n\ttabbed\nlast").unwrap();
        assert_eq!(text, "  indented\n\ttabbed\nlast");
    }

    /// Recognition has to cover the hosted spellings, not just the bare id.
    #[test]
    fn anthropic_models_are_recognised_in_the_hosted_spellings() {
        for model in [
            "claude-sonnet-4-6",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "claude-3-5-sonnet@20240620",
            "CLAUDE-OPUS-4",
        ] {
            assert!(is_anthropic(model), "{model} should read as Anthropic");
        }
        // Unknown is not Anthropic — the direction that avoids claiming to be
        // Claude on a model that is not.
        for model in ["deepseek-flash", "gpt-4o", "gemini-2.5-pro", "", "llama-3.3"] {
            assert!(!is_anthropic(model), "{model} should not read as Anthropic");
        }
    }

    #[test]
    fn inline_text_wins_over_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("identity.md");
        std::fs::write(&file, "from the file").unwrap();

        let identity = load(Some("from the flag"), Some(&file)).unwrap();
        assert_eq!(
            identity,
            Identity::Text {
                text: "from the flag".into(),
                allowed_tools: None
            }
        );
    }

    #[test]
    fn a_blank_inline_is_an_error() {
        let err = load(Some("   "), None).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    /// An explicitly named file is a request, so a bad one is an error — unlike
    /// the default path, whose absence is ordinary.
    #[test]
    fn an_explicit_file_that_is_missing_or_blank_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.md");
        let err = load(None, Some(&missing)).unwrap_err();
        assert!(err.contains("cannot read identity file"), "{err}");

        let blank = dir.path().join("blank.md");
        std::fs::write(&blank, "---\nAllowedTools: Read\n---\n").unwrap();
        let err = load(None, Some(&blank)).unwrap_err();
        assert!(err.contains("no prompt"), "{err}");
    }

    #[test]
    fn an_explicit_file_carries_its_allowed_tools() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("identity.md");
        std::fs::write(&file, "---\nAllowedTools: Write, Read\n---\nYou are a parrot.").unwrap();
        let identity = load(None, Some(&file)).unwrap();
        assert_eq!(
            identity,
            Identity::Text {
                text: "You are a parrot.".into(),
                allowed_tools: Some(vec!["Write".into(), "Read".into()])
            }
        );
    }

    /// The states the default path can be in.
    ///
    /// Driven through `load_at` with the path passed in, rather than by setting
    /// `CATBUS_IDENTITY_FILE`: edition 2024 makes `set_var` `unsafe`, and this
    /// crate denies `unsafe` outright, so the environment can only be read, never
    /// written, from inside a test.
    #[test]
    fn the_default_file_is_absent_blank_a_prompt_or_broken() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("chosen.md");

        // Absent: built-in behaviour.
        assert_eq!(load_at(None, None, &target).unwrap(), Identity::Auto);

        // Blank: send nothing.
        std::fs::write(&target, "   \n").unwrap();
        assert_eq!(load_at(None, None, &target).unwrap(), Identity::Omitted);

        // Front matter with no body is blank too, so it also means "nothing".
        std::fs::write(&target, "---\nAllowedTools: Read\n---\n").unwrap();
        assert_eq!(load_at(None, None, &target).unwrap(), Identity::Omitted);

        // A prompt: use it.
        std::fs::write(&target, "You are a parrot.").unwrap();
        assert_eq!(
            load_at(None, None, &target).unwrap(),
            Identity::Text {
                text: "You are a parrot.".into(),
                allowed_tools: None
            }
        );

        // Broken front matter is reported rather than treated as not-a-prompt.
        std::fs::write(&target, "---\nAllowedTools: Read\nno closing rule\n").unwrap();
        let err = load_at(None, None, &target).unwrap_err();
        assert!(err.contains("never closed"), "{err}");
    }

    /// A default file that exists but cannot be read is an error, not a silent
    /// fall back to the built-in prompt — otherwise a permissions problem looks
    /// like the agent ignoring the operator's file.
    #[test]
    fn an_unreadable_default_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file is expected: present, unreadable as a file.
        let target = dir.path().join("identity.md");
        std::fs::create_dir(&target).unwrap();
        let err = load_at(None, None, &target).unwrap_err();
        assert!(err.contains("cannot read identity file"), "{err}");
    }

    #[test]
    fn the_built_in_path_prefers_xdg_then_home() {
        let xdg = path_from(Some("/xdg".into()), Some("/home/u".into()));
        assert_eq!(xdg, PathBuf::from("/xdg/tab-atelier/catbus-agent/identity.md"));

        let home = path_from(None, Some("/home/u".into()));
        assert_eq!(
            home,
            PathBuf::from("/home/u/.config/tab-atelier/catbus-agent/identity.md")
        );

        // No environment at all still resolves, rather than panicking on start-up.
        assert_eq!(
            path_from(None, None),
            PathBuf::from(".config/tab-atelier/catbus-agent/identity.md")
        );
    }

    /// `path` is the only reader of the environment, and it must not invent a
    /// value from an empty string — an empty `XDG_CONFIG_HOME` is common in
    /// environments that set it to nothing.
    #[test]
    fn an_empty_environment_variable_is_treated_as_unset() {
        assert_eq!(non_empty_env("CATBUS_IDENTITY_FILE_THAT_IS_NOT_SET"), None);
    }
}
