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
/// The width every description starts at.
///
/// Measured across both tables, not one: `/help` is read as a single list, and
/// two columns that happen to differ would look like a mistake in whichever
/// section came second.
fn help_column() -> usize {
    COMMANDS
        .iter()
        .map(|command| command.help_name().len())
        .chain(SHELL_HELP.iter().map(|(name, _)| name.len()))
        .max()
        .unwrap_or_default()
        + COLUMN_GAP
}

/// `/help`: the heading and entries of each section, in the order they are shown.
///
/// Two tables because a `!` line is not a slash command — it has no action to
/// take, it is handed to the shell — so it is not in [`COMMANDS`] and would not
/// otherwise appear at all. A feature nobody can find is a feature nobody uses.
#[must_use]
fn help_sections() -> [(&'static str, Vec<(String, &'static str)>); 2] {
    [
        (
            "slash commands:",
            COMMANDS
                .iter()
                .map(|command| (command.help_name(), command.description))
                .collect(),
        ),
        (
            "your own shell:",
            SHELL_HELP
                .iter()
                .map(|(name, description)| ((*name).to_owned(), *description))
                .collect(),
        ),
    ]
}

/// The help text, ready to print.
#[must_use]
pub fn help_text() -> String {
    let column = help_column();
    let mut lines = Vec::new();
    for (heading, entries) in help_sections() {
        lines.push(heading.to_owned());
        for (name, description) in entries {
            lines.push(format!("  {name:<column$}{description}"));
        }
        lines.push(String::new());
    }
    // Said once, under both tables, because it applies to the shell lines and is the one
    // thing about them that cannot be guessed from the list.
    lines.push(String::from(
        "  Ctrl-B and Ctrl-C stop waiting for a `!` command, and stop it.",
    ));
    let mut text = lines.join("\n");
    text.push_str("\n\n");
    text
}

/// The `!` lines, in the same shape as [`COMMANDS`] so `/help` reads as one list.
///
/// The recommendation is on the entry it applies to rather than in a sentence
/// after the table, because that is where someone reading for "how do I run this
/// in the background" is already looking.
const SHELL_HELP: &[(&str, &str)] = &[
    ("!<cmd>", "Run it, show the output, then tell the model what it printed"),
    ("!!<cmd>", "Run it and show the output, and tell the model nothing"),
    (
        "!<cmd> &",
        "The same, in the background — recommended: the prompt stays free",
    ),
    ("!!<cmd> &", "In the background, and silent"),
];

/// A line the operator typed to run on their own shell.
///
/// Two sigils, and the second `!` is the whole point: it says the model is not
/// to hear about this one. Checked here, beside the slash table and before the
/// fall-through that makes a line a prompt, because a `!` line must never reach
/// the model as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `!cmd` — run it, show the output, and tell the model what it printed.
    Tell,
    /// `!!cmd` — run it and show the output, and tell the model nothing.
    Silent,
}

/// What a `!` line asks for: what to run, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLine {
    /// The command, with the `!`, the `!!` and any background marker taken off.
    pub command: String,
    /// Whether the model is told when it finishes.
    pub tell_model: bool,
    /// Whether it is to run in the background — a trailing `&`.
    pub background: bool,
}

/// The shell line `input` names, or `None` if it is not one.
///
/// `None` for anything that does not open with `!`, which is what leaves the
/// line to be a prompt or a slash command; and `None` for a bare `!` with no
/// command after it, which is a mistake to report rather than an empty command
/// to run.
///
/// The background marker is a trailing `&` that is a word of its own — `cmd &`
/// rather than `cmd && something`, and rather than `cmd 2>&1`, where the `&` is
/// part of the redirection. Requiring whitespace (or the whole command to be
/// just `&`) is what tells those apart, since a bare trailing `&` in shell is
/// exactly the background operator.
#[must_use]
pub fn shell(input: &str) -> Option<ShellLine> {
    let input = input.trim();
    let (kind, rest) = match input.strip_prefix("!!") {
        Some(rest) => (Shell::Silent, rest),
        None => (Shell::Tell, input.strip_prefix('!')?),
    };
    let rest = rest.trim();
    let (command, background) = match rest.strip_suffix('&') {
        // `&` alone was never a command, and `&&` is not a background marker.
        Some(head) if head.ends_with(char::is_whitespace) => (head.trim(), true),
        _ => (rest, false),
    };
    // A sigil with nothing after it, and `&` with nothing before it, are both
    // mistakes to report rather than commands to attempt: `&` on its own is a
    // syntax error to the shell, so running it would produce an error rather
    // than an answer. `make &&` is left alone — its ampersands are part of the
    // command, and only a trailing one with whitespace in front of it is the
    // background operator.
    if command.is_empty() || command.trim_matches('&').trim().is_empty() {
        return None;
    }
    Some(ShellLine {
        command: command.to_owned(),
        tell_model: kind == Shell::Tell,
        background,
    })
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
    fn a_bang_line_is_a_shell_command_and_says_who_hears_about_it() {
        // One `!` tells the model, two do not. The distinction is the operator's
        // to make, so both spellings have to keep meaning what they say.
        let told = shell("!make test").expect("a shell line");
        assert_eq!(told.command, "make test");
        assert!(told.tell_model);
        assert!(!told.background);

        let quiet = shell("!!make test").expect("a shell line");
        assert_eq!(quiet.command, "make test");
        assert!(!quiet.tell_model);
        assert!(!quiet.background);

        // The command is taken verbatim, so its own quoting and flags survive.
        let quoted = shell("!git commit -m 'a message'").expect("a shell line");
        assert_eq!(quoted.command, "git commit -m 'a message'");
    }

    #[test]
    fn a_trailing_ampersand_asks_for_the_background() {
        let background = shell("!npm test &").expect("a shell line");
        assert_eq!(background.command, "npm test");
        assert!(background.background);
        assert!(background.tell_model, "the sigil still decides who is told");

        let quiet = shell("!!npm test &").expect("a shell line");
        assert!(quiet.background);
        assert!(!quiet.tell_model);

        // And whitespace before the `&` is not required to be a single space.
        assert_eq!(shell("!ls\t&").expect("a shell line").command, "ls");
    }

    #[test]
    fn an_ampersand_that_is_not_a_background_marker_is_left_alone() {
        // These all contain an `&` that a shell would read as part of the
        // command, so splitting it off would change what runs.
        for line in [
            "!make && echo done",
            "!echo a & b",
            "!cmd > out 2>&1",
            "!f() { echo hi; }; f",
        ] {
            let parsed = shell(line).expect("a shell line");
            assert!(!parsed.background, "{line:?} is not a background request");
            assert_eq!(
                parsed.command,
                line.trim_start_matches('!'),
                "{line:?} keeps its ampersand"
            );
        }
    }

    #[test]
    fn what_is_not_a_shell_line_stays_a_prompt() {
        for not_a_shell_line in [
            "",       // nothing at all
            "!",      // a sigil and no command
            "!!",     // both sigils and no command
            "!   ",   // a sigil and only spaces
            "!! &",   // nothing to run, in the background
            "ls -la", // an ordinary prompt
            "/help",  // a slash command, which is checked separately
            // A `!` that is not the first character is an ordinary sentence.
            "wow! that worked",
        ] {
            assert_eq!(
                shell(not_a_shell_line),
                None,
                "{not_a_shell_line:?} is not a shell line"
            );
        }
    }

    #[test]
    fn a_shell_line_is_never_also_a_slash_command() {
        // The two parsers are consulted in order, so a line that both accepted
        // would depend on which ran first. They must not overlap at all.
        for line in ["!make test", "!!make test", "!npm test &", "!plan"] {
            assert!(shell(line).is_some(), "{line:?} is a shell line");
            assert_eq!(lookup(line), None, "{line:?} is not a slash command");
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
        // Checked against the text as printed, not against a line rebuilt the same
        // way `help_text` builds it — a test that reconstructs the output agrees
        // with itself whatever the output says.
        let help = help_text();
        let lines: Vec<&str> = help.lines().collect();
        let mut columns = Vec::new();
        let mut index = 0;
        for (heading, entries) in help_sections() {
            assert_eq!(lines.get(index).copied(), Some(heading), "{heading:?} is not a heading");
            index += 1;
            for (name, description) in &entries {
                let line = lines
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| panic!("{name:?} has no line in the help text:\n{help}"));
                assert!(line.starts_with("  "), "{line:?} is not indented");
                assert!(line.ends_with(description), "{line:?} does not end in {description:?}");
                let column = line.len() - description.len();
                assert!(column > 2, "{line:?} has no gap between the name and the description");
                // The name is what sits between the indent and the description.
                assert_eq!(
                    line[2..column].trim_end(),
                    *name,
                    "{line:?} does not begin with {name:?}"
                );
                columns.push(column);
                index += 1;
            }
            // A blank line ends each section, so the two tables read as two lists.
            assert_eq!(lines.get(index).copied(), Some(""), "{heading:?} is not ended");
            index += 1;
        }
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "descriptions start at different columns, across the sections as well as within \
             them: {columns:?}\n{help}"
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
