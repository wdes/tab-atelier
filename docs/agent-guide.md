# Using the agent

Everything `catbus-agent` does from the operator's side: the commands, the permission
modes, the tools it has, and what to do when it refuses. Written for someone who has the
package installed and no checkout to read.

    catbus-agent                     # a session in this directory, in this terminal
    catbus-agent --resume <id>       # continue an earlier session
    catbus-agent --help              # every flag

The agent resolves its relay from `~/.config/tab-atelier/preferences.json`, so in a tab it
needs no flags at all. Run `catbus-agent --check` first if it will not start — that names
the library or package that is missing.

## Talking to it

Type a prompt and press Enter. While a long answer is arriving you can keep typing: press
Enter again and the line queues, and the status row shows `· 1 queued`. Nothing starts
until the current turn finishes, so the transcript stays in the order you asked things.
`Ctrl-C` aborts the turn and clears anything queued — the queued lines are in your history,
so the up arrow brings them back.

`Ctrl-D` leaves. `Ctrl-U`, `Ctrl-W`, `Ctrl-A`, `Ctrl-E` and the arrows work as they do in
a shell, and history is per session.

## Commands

Every command is listed by `/help`, and the list is generated from the same table the code
dispatches on — so a command cannot exist without being documented.

| Command | What it does |
|---|---|
| `/help` | This list. |
| `/clear` | Start a fresh session here. The old transcript stays on disk; the message tells you its id, and `/resume <id>` returns to it. The screen and scrollback are wiped, so the session looks new. |
| `/plan` | Propose instead of acting: write, edit, shell and the package tools describe what they would do. |
| `/auto` | A judge checks each write, edit, shell and package command before it runs, and refuses the severe ones. |
| `/noplan` (alias `/noauto`) | Allow everything. The default. |
| `/model` | Show the model this session runs as. |
| `/model <name>` | Switch it. Remembered for the session, so reopening continues with it. |
| `/rename <name>` | Name the session, so `/resume` lists something recognisable. |
| `/resume` | List this directory's earlier sessions, with the first thing you asked each one. |
| `/resume <id>` | Switch to one, in place. |
| `/exit` (alias `/quit`) | Leave. Works even while a turn is running. |

## Permission modes

Three modes, and the banner says which you are in when the session starts, because a write
that went through when you expected it to be checked is the thing you need to explain.

**`open`** — nothing is checked. The default.

**`plan`** — the agent may read anything and change nothing. A write, an edit, a shell
command, a `composer run`, a commit: each is refused with a message saying to describe it
instead. Asking a question still works, because asking is not an action.

**`auto`** — before each write, edit, shell or package command, a second model reads the
proposed action and scores its severity from 0 to 100. Above 50 it is refused, and the
refusal reaches the agent with the severity and the reason. Every check is recorded — in
the log, and in the tool result the agent sees — so an allowed action leaves a trace.
Without that, a gate doing its job and a gate that never ran look identical, which is
exactly how "auto mode does nothing" gets reported about a working gate.

A judge that cannot be read refuses rather than allows: a failed check is not permission.

The mode is remembered per session, so it survives a tab being reopened, and `--gate auto`
pins it for a launch.

## What it can do

| Tool | What it is for |
|---|---|
| `Read` | Read a file. Absolute or relative to the working directory. |
| `FileTree` | List a directory, 1–6 levels deep, respecting ignore rules. |
| `Write` | Create or replace a file. |
| `Edit` | Exact-string replacement in an existing file. |
| `Bash` | Run a shell command. |
| `Tasks` | The agent's own task list for this directory — `add`, `list`, `start`, `done`, `block`, `drop`, `note`, `clear`. On disk, so it survives a long conversation; a sub-agent in the same directory shares it. |
| `PHPUnit` | Run this project's PHPUnit and get structured results: counts, and one entry per failure with its test, `file:line`, message and diff already separated. |
| `Composer` | `install`, `update`, or `run` a script from composer.json. `scripts` lists what the project defines, with the author's descriptions where there are any. Always non-interactive. |
| `Bun` | `run` a script from package.json, `install`, or `scripts` to list them. |

Ask for `scripts` when you want to know what a project can run — it reads the manifest
directly, so it answers even where the package manager is not installed. A `run` with a name
the project does not define is refused with the real names, rather than being passed through
to the package manager, whose error would say only what it could not find.
| `GitStatus` | What is uncommitted: branch, ahead/behind, and the staged, unstaged, untracked and conflicted paths. Read-only. |
| `GitCommit` | Commit a named set of files. It stages nothing you did not name; commits by path, so anything else already staged stays staged. |
| `ListAgents` | Other agents running on this machine. |
| `Delegate` | Ask one of them something, and wait. |
| `Spawn` | Start a new agent for one task, take its reply, and stop it. |
| `AskUserQuestion` | Ask you to choose, when a decision is yours. |

`PHPUnit`, `Composer`, `Bun`, `GitCommit` and `Spawn` all run project code, so `auto` judges
them and `plan` refuses them. `Read`, `FileTree`, `GitStatus` and `ListAgents` change
nothing and are never judged. `Tasks` writes only to the agent's own state directory, so it
is allowed even in plan mode — a plan is what plan mode is for.

**There is no push.** Committing is the agent's; publishing is yours.

## Being asked a question

The agent stops and asks rather than guessing when a choice is yours — which of two
schemas, whether a destructive step is wanted. The question appears with numbered options:

    Which schema should the migration use?
      1. normalised — a table for line items
      2. json column — faster to ship
    answer with a number, or the label itself

Answer with `2`, with `json column`, or `1,3` for a question that takes several. An answer
it cannot read is reported rather than guessed at. If you are not there, it gives up and
tells the agent so, and the agent is expected to decide and say what it assumed.

## When something goes wrong

A failed API call is reported as the provider's own sentence, with its error type, not as a
wall of JSON. A provider that is busy says so and says that the turn can simply be repeated.

If a turn fails, the session continues: the error is printed and you can carry on. Errors
that end the program are reserved for the terminal itself.

Two things that look like faults and are not:

* **A leading `mode open — nothing is checked` line.** That is the banner confirming the
  mode, not a warning.
* **`~1,200 tokens in` on the status row.** That is the local count of what was sent, marked
  `~` because it is an estimate; the real figure comes from the provider and appears on the
  line under a finished answer.

If the agent is not running at all, the tab's right-click menu entry is dimmed and says an
agent already runs there — start one agent per tab, not two.
