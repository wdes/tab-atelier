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

A prompt can be several lines. **Shift+Enter** (or **Alt+Enter**) inserts a newline instead
of sending, so you can lay out a list or a small script and send it whole; Enter is still
what sends. A newline does not submit, so a pasted block of several lines arrives as one
message rather than sending itself at the first line break.

Anything longer than the terminal is wide wraps onto the next row, and the prompt area
reserves room for several rows, so a long or multi-line prompt stays readable while you
write it. Past that it scrolls inside the prompt area, keeping the row you are typing on.

Shift+Enter needs the terminal to report it distinctly, which most modern ones do; where it
does not, Alt+Enter always works.

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

## Your own shell

A line starting with `!` runs in your shell, here, and you watch it as it happens. It is not a
prompt and never reaches the model as text.

| Line | What it does |
|---|---|
| `!<cmd>` | Run it, show the output, then tell the model what it printed. |
| `!!<cmd>` | Run it and show the output. The model is told nothing. |
| `!<cmd> &` | The same, in the background. |
| `!!<cmd> &` | In the background, and silent. |

**Prefer the background.** A command run this way keeps the prompt free, so you can keep reading
the output, run something else, or carry on talking to the model while it runs — and a slow one
cannot hold the session still. The model is told how it ended whether or not it was backgrounded,
so `!make test &` is almost always what you want over `!make test`. The only cost is that you have
to be looking when the notice arrives.

A foreground command owns the prompt until it finishes, and the status row says so. Press
**Ctrl-B** to stop waiting for it — it carries on in the background — or **Ctrl-C** to stop the
command itself. Nothing is lost either way; a background command still reports when it ends.

Two things to know about the model's side of it. The first: what the model is told is that *you*
ran the command, quoting it, with the exit status and the output — so it takes the result into
account without ever believing one of its own tools produced it. The second: if the model is
already mid-turn, the notice waits for its turn rather than interrupting it, and if a turn is not
running, the notice starts one.

While the model works, its reasoning streams above the prompt in grey. It is there to be watched
and clears when the answer arrives — the transcript stays the answer.

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
| `Git` | One tool, eight actions — `show`, `commit`, `tag`, `history`, three `worktree` verbs and `push`. See below. |
| `ListAgents` | Other agents running on this machine. |
| `Delegate` | Ask one of them something, and wait. |
| `Spawn` | Start a new agent for one task, take its reply, and stop it. |
| `SSH` | Run one non-interactive command on a host (`command`), or report a host's public keys (`keyscan`). Only a host and whether to forward the agent can be set — see below. |
| `AskUserQuestion` | Ask you to choose, when a decision is yours. |

Ask a package tool for `scripts` when you want to know what a project can run — it reads the
manifest directly, so it answers even where the package manager is not installed. A `run` with a
name the project does not define is refused with the real names, rather than being passed through to
the package manager, whose error would say only what it could not find.

### What SSH may do

`SSH` is two actions and both take almost nothing:

    SSH { action: command, host: dc1.example.org, command: "uptime" }
    SSH { action: keyscan, host: dc1.example.org }

Only `host` — a name, an address, or either with a port — plus `jump` and `forward_agent` may be set.
There is no user, no extra ssh option, no key selection and no tunnel, so none of those is reachable
through it. The login is whatever your own ssh config says.

`jump` routes the connection through another host (`ssh -J`), for a machine only reachable from a
bastion. It needs **its own grant**: the session's `AllowedJumpHosts` must list it, and that list is
opt-in — `AllowedHosts` permits destinations, not routes to them. The refusal when it is missing names
the line to add.

`command` runs one non-interactive command and returns its output. It cannot prompt: ssh runs with
no terminal and in batch mode, so a command that needs a password or an answer fails instead of
hanging.

`keyscan` reports the host's public keys. It trusts nothing on its own — pass `trust: true` to add
them to `~/.ssh/known_hosts`, and note that they are whatever answered at that address, so compare
the fingerprints before relying on them. A host that is not in `known_hosts` gets a refusal that
points at `keyscan`, so running a command never trusts a host as a side effect.

`forward_agent` forwards your SSH agent for that connection, which means a command on the host can
use your keys. Off unless asked; that is the whole point of the flag and the whole risk of it.

`PHPUnit`, `Composer`, `Bun`, the writing `Git` actions, `SSH` and `Spawn` all run project code or
change things, so `auto` judges them and `plan` refuses them. `Read`, `FileTree`, `ListAgents` and the
reading `Git` actions — `show`, `history`, `worktree-list` — change nothing and are never judged. `Tasks` writes only to the agent's own state directory, so it
is allowed even in plan mode — a plan is what plan mode is for.

**`push` names its destination.** A remote and a branch are both required, so a publish is never
implicit — and in a session under `plan` or `auto` it is refused or judged like anything else that
changes the world.

## Being asked a question

The agent stops and asks rather than guessing when a choice is yours — which of two
schemas, whether a destructive step is wanted. The question appears in the scrollback, and
the choices take over the prompt line as a tick-box list:

    Which schema should the migration use?
    ▸[ ] normalised  [ ] json column

Move with the arrow keys (or `j`/`k`), tick with space, and press enter to send. On a
question that takes several, tick as many as you like; on one that takes a single answer,
ticking a second clears the first. `tab` moves to the next question when the agent asked a
few at once, and `↑↓` wrap around, so you never have to reverse direction to get back.

Press `n` to attach a note to the reply — the one thing a fixed list of labels cannot
express ("the second one, but only if the migration has already been applied"). The note is
sent alongside the choices, never instead of them. Escape leaves the note field with what you
typed intact; enter sends the whole reply, note and all. `ctrl-c` aborts the turn instead of
answering.

Enter with nothing ticked and nothing written is refused, rather than sent: an empty reply
looks the same as a slip of the finger. To say "none of these", write it in a note.

If you are not there, the question times out, the tool reports that nobody answered, and the
agent is expected to decide and say what it assumed. The same question can also be answered
from a socket client, which sees the question and sends the choices back.

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
