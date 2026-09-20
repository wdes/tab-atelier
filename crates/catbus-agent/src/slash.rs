// SPDX-License-Identifier: MPL-2.0

//! The REPL's slash commands, listed once.
//!
//! Every command the REPL understands is an entry in [`COMMANDS`], and that
//! table is the only place one exists: `/help` is generated from it and the
//! dispatch looks the typed word up in it. So a command cannot be reachable
//! without being documented, nor documented with a description it does not
//! behave like — the two are the same literal.
//!
//! A word is matched whole and folded to ASCII case. Whole, because `/plan the
//! refactor` is a prompt: only a command whose `arg` is `Some` may be followed
//! by anything. Folded, because the mode words already were — they resolve
//! through [`crate::tools::parse_gate`], which the socket's `set_gate` goes
//! through too, and a word that selects a mode over the socket must not become
//! a plain prompt when it is typed one letter louder.

/// What the REPL does about a command it has looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Print the generated command list.
    Help,
    /// Forget the conversation and start a fresh session here.
    Clear,
    /// Select a gate mode. The word resolves through
    /// [`crate::gate_command`], so the REPL and the socket keep naming modes
    /// the same way.
    Gate,
    /// Rename the current session.
    Rename,
    /// List this cwd's previous sessions, or switch to the one named.
    Resume,
    /// Show or choose the model this session runs as.
    Model,
    /// Leave the REPL, as Ctrl-D does.
    Exit,
}

/// One slash command.
#[derive(Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// The word that selects it, slash included, spelled as it is typed.
    pub name: &'static str,
    /// What it does, in the operator's words. Shown by `/help`.
    pub description: &'static str,
    /// Other words that select it too, slash included — `/quit` for `/exit`.
    /// Help lists them as part of the one entry they share.
    pub aliases: &'static [&'static str],
    /// The argument it accepts, spelled as help shows it (`<name>` for
    /// `/rename <name>`); `None` when it takes none, in which case anything
    /// typed after the name makes the line a prompt instead of a command.
    pub arg: Option<&'static str>,
    /// What the REPL does when it is selected.
    pub action: Action,
}

impl SlashCommand {
    /// The command as `/help` spells it: the name, and its argument when it
    /// takes one.
    #[must_use]
    pub fn help_name(&self) -> String {
        self.arg
            .map_or_else(|| self.name.to_owned(), |arg| format!("{} {arg}", self.name))
    }

    /// Every word that selects this command: the name and its aliases.
    pub fn words(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }

    /// Whether `word` names this command, directly or through an alias.
    #[must_use]
    pub fn answers_to(&self, word: &str) -> bool {
        self.words().any(|name| name.eq_ignore_ascii_case(word))
    }
}

const HELP_NAME: &str = "/help";
const HELP_DESCRIPTION: &str = "show this list";

const CLEAR_NAME: &str = "/clear";
const CLEAR_DESCRIPTION: &str = "forget the conversation, start a fresh session here";

const PLAN_NAME: &str = "/plan";
const PLAN_DESCRIPTION: &str = "plan only - write/edit/bash propose instead of acting";

const AUTO_NAME: &str = "/auto";
const AUTO_DESCRIPTION: &str = "ask a judge before each write/edit/bash";

const NOPLAN_NAME: &str = "/noplan";
const NOPLAN_DESCRIPTION: &str = "allow everything (same as /noauto)";
// One mode, two words for it. A second entry would suggest a state that does not
// exist, and `/noauto` is the spelling the REPL has always accepted. The socket's
// `set_gate` additionally takes `open`, but that is the socket's vocabulary, not
// the REPL's — adding it here would put a word in the REPL that `help_text` does
// not document, which is the drift this table exists to prevent.
const NOPLAN_ALIASES: &[&str] = &["/noauto"];

const RENAME_NAME: &str = "/rename";
const RENAME_ARG: &str = "<name>";
const RENAME_DESCRIPTION: &str = "rename the current session";

const RESUME_NAME: &str = "/resume";
const RESUME_ARG: &str = "<id>";
const RESUME_DESCRIPTION: &str = "list previous sessions in this cwd, or switch to one in-place";

const MODEL_NAME: &str = "/model";
const MODEL_ARG: &str = "<name>";
const MODEL_DESCRIPTION: &str = "show the model this session runs as, or switch to another";

const EXIT_NAME: &str = "/exit";
const EXIT_DESCRIPTION: &str = "quit (same as /quit, or Ctrl-D)";
const EXIT_ALIASES: &[&str] = &["/quit"];

/// Every slash command, in the order `/help` prints them.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: HELP_NAME,
        description: HELP_DESCRIPTION,
        aliases: &[],
        arg: None,
        action: Action::Help,
    },
    SlashCommand {
        name: CLEAR_NAME,
        description: CLEAR_DESCRIPTION,
        aliases: &[],
        arg: None,
        action: Action::Clear,
    },
    SlashCommand {
        name: PLAN_NAME,
        description: PLAN_DESCRIPTION,
        aliases: &[],
        arg: None,
        action: Action::Gate,
    },
    SlashCommand {
        name: AUTO_NAME,
        description: AUTO_DESCRIPTION,
        aliases: &[],
        arg: None,
        action: Action::Gate,
    },
    SlashCommand {
        name: NOPLAN_NAME,
        description: NOPLAN_DESCRIPTION,
        aliases: NOPLAN_ALIASES,
        arg: None,
        action: Action::Gate,
    },
    SlashCommand {
        name: RENAME_NAME,
        description: RENAME_DESCRIPTION,
        aliases: &[],
        arg: Some(RENAME_ARG),
        action: Action::Rename,
    },
    SlashCommand {
        name: RESUME_NAME,
        description: RESUME_DESCRIPTION,
        aliases: &[],
        arg: Some(RESUME_ARG),
        action: Action::Resume,
    },
    SlashCommand {
        name: MODEL_NAME,
        description: MODEL_DESCRIPTION,
        aliases: &[],
        arg: Some(MODEL_ARG),
        action: Action::Model,
    },
    SlashCommand {
        name: EXIT_NAME,
        description: EXIT_DESCRIPTION,
        aliases: EXIT_ALIASES,
        arg: None,
        action: Action::Exit,
    },
];

/// The blank space between a name and its description in `/help`. Five is what
/// the block was hand-padded to before it was generated.
const COLUMN_GAP: usize = 5;

/// The `/help` block, generated from [`COMMANDS`].
///
/// The column the descriptions start at is measured from the table rather than
/// written down, so a new command — or a longer argument — widens the column
/// instead of knocking the block out of true.
#[must_use]
pub fn help_text() -> String {
    let column = COMMANDS
        .iter()
        .map(|command| command.help_name().len())
        .max()
        .unwrap_or_default()
        + COLUMN_GAP;
    let mut lines = vec![String::from("slash commands:")];
    for command in COMMANDS {
        let name = command.help_name();
        lines.push(format!("  {name:<column$}{}", command.description));
    }
    let mut text = lines.join("\n");
    text.push_str("\n\n");
    text
}

/// The command `input` names, with the argument typed after it — trimmed, and
/// empty when there was none.
///
/// `None` when `input` is not a slash command at all, which is what leaves the
/// line to become a prompt for the model.
#[must_use]
pub fn lookup(input: &str) -> Option<(&'static SlashCommand, &str)> {
    let input = input.trim();
    let (word, arg) = match input.split_once(' ') {
        Some((word, arg)) => (word, arg.trim()),
        None => (input, ""),
    };
    let command = COMMANDS.iter().find(|command| command.answers_to(word))?;
    if command.arg.is_none() && !arg.is_empty() {
        return None;
    }
    Some((command, arg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_commands_answer_to_the_same_word() {
        // A duplicate would make one of the two unreachable — the lookup returns
        // whichever comes first — and the loser would still be listed in help,
        // so nothing would look wrong. Compared case-insensitively because the
        // lookup folds case, which makes `exit` and `EXIT` the same word to it.
        let words: Vec<&str> = COMMANDS.iter().flat_map(SlashCommand::words).collect();
        for (i, word) in words.iter().enumerate() {
            for other in &words[i + 1..] {
                assert!(
                    !word.eq_ignore_ascii_case(other),
                    "{word} and {other} would be the same word to the lookup"
                );
            }
        }
    }

    #[test]
    fn every_command_is_reachable_by_name_and_by_alias() {
        // The auto-detection the table exists for: each entry finds itself, so a
        // command cannot be added under a name the lookup does not match.
        for command in COMMANDS {
            for word in command.words() {
                assert_eq!(
                    lookup(word).map(|(found, _)| found),
                    Some(command),
                    "{word} does not select its own command"
                );
            }
        }
    }

    #[test]
    fn a_command_that_takes_no_argument_is_matched_whole() {
        // `/plan the refactor` is a prompt: only a command that declares an
        // argument may be followed by one.
        for command in COMMANDS.iter().filter(|command| command.arg.is_none()) {
            assert_eq!(lookup(&format!("{} trailing", command.name)), None, "{}", command.name);
        }
        assert!(lookup("/auto trailing").is_none(), "the gate words take no argument");
    }

    #[test]
    fn a_word_the_table_does_not_list_is_not_a_command() {
        // The other half of the contract: anything else reaches the model as a
        // prompt, never as a slash command that quietly did something.
        for not_a_command in [
            "/automatic", // a longer word that starts like a command
            "/planning",
            "/noplan/x",
            "/pla",
            "noauto", // the alias without its slash is not the command
            "exit",
            "plain text",
            "/",
            "",
            "  ",
        ] {
            assert_eq!(lookup(not_a_command), None, "{not_a_command:?} should not be a command");
        }
    }

    #[test]
    fn an_argument_is_the_text_after_the_first_space_trimmed() {
        // The REPL hands the argument on as-is, so the splitting belongs here:
        // `/rename  spaced  out ` names `spaced  out`.
        assert_eq!(
            lookup("/rename spaced  out ").map(|(command, arg)| (command.name, arg)),
            Some((RENAME_NAME, "spaced  out"))
        );
        assert_eq!(
            lookup("/rename").map(|(command, arg)| (command.name, arg)),
            Some((RENAME_NAME, ""))
        );
        assert_eq!(
            lookup("/resume").map(|(command, arg)| (command.name, arg)),
            Some((RESUME_NAME, ""))
        );
        assert_eq!(
            lookup("/resume abc-123 ").map(|(command, arg)| (command.name, arg)),
            Some((RESUME_NAME, "abc-123"))
        );
    }

    #[test]
    fn help_documents_every_command_on_its_own_line() {
        // The drift this module exists to prevent: a command the operator can
        // type but cannot find, or one described as something it does not do.
        let help = help_text();
        assert!(help.starts_with("slash commands:\n  "), "unexpected header:\n{help}");
        assert!(help.ends_with("\n\n"), "the block should end on a blank line:\n{help}");
        for command in COMMANDS {
            let help_name = command.help_name();
            // Matched on the whole name column: `contains` alone would let
            // `/plan` pass on `/noplan`'s line.
            let line = help.lines().find(|line| line.starts_with(&format!("  {help_name}")));
            let Some(line) = line else {
                panic!("{help_name} has no entry in help:\n{help}");
            };
            assert!(
                line.ends_with(command.description),
                "the entry for {help_name} does not carry its description: {line:?}"
            );
        }
    }

    #[test]
    fn help_lists_one_line_per_command_at_one_column() {
        // The shape the hand-written block had, kept: two spaces of indent, and
        // every description starting at the same column.
        let help = help_text();
        let lines: Vec<&str> = help.lines().skip(1).filter(|line| !line.is_empty()).collect();
        assert_eq!(lines.len(), COMMANDS.len(), "one line per command:\n{help}");
        let columns: Vec<usize> = COMMANDS
            .iter()
            .zip(&lines)
            .map(|(command, line)| {
                assert!(line.starts_with("  "), "{line:?} is not indented");
                let column = line.len() - command.description.len();
                assert!(
                    line.ends_with(command.description),
                    "{line:?} does not end in its description"
                );
                assert!(column > 2, "{line:?} has no gap between the name and the description");
                column
            })
            .collect();
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "descriptions start at different columns: {columns:?}\n{help}"
        );
    }

    #[test]
    fn the_words_for_allow_everything_are_one_entry() {
        // `/noplan` and `/noauto` name the same mode, so they share an entry:
        // two entries would print the same behaviour twice, and a reader would
        // take them for two states.
        assert_eq!(lookup("/noplan").map(|(command, _)| command.name), Some(NOPLAN_NAME));
        assert_eq!(lookup("/noauto").map(|(command, _)| command.name), Some(NOPLAN_NAME));
    }

    #[test]
    fn a_gate_command_names_the_mode_the_socket_parses() {
        // A gate command is only as good as its name: the REPL resolves it
        // through `gate_command`, which delegates to the same `parse_gate` the
        // socket's `set_gate` uses. An alias that resolved to another mode would
        // print one thing and set another.
        for command in COMMANDS.iter().filter(|command| command.action == Action::Gate) {
            let gate = crate::gate_command(command.name);
            assert!(gate.is_some(), "{} selects no gate", command.name);
            for alias in command.aliases {
                assert_eq!(
                    crate::gate_command(alias),
                    gate,
                    "{alias} and {} disagree",
                    command.name
                );
            }
        }
    }
}
