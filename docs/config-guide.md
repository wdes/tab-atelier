# Configuring the agent

Where its files live, how to give it a different identity, and how to choose which tools it
has. Written for someone with the package installed and no checkout.

## Files

| Path | What it is | Set by |
|---|---|---|
| `~/.config/tab-atelier/preferences.json` | The relay endpoint. The agent reads this for its URL and token, so a tab needs no flags. | `tab-atelier` writes it; `/etc/tab-atelier/preferences.json` is the fallback for a fresh install. |
| `~/.config/tab-atelier/catbus-agent/identity.md` | The system prompt. See below. | You. |
| `~/.config/tab-atelier/catbus-agent/tools.json` | Which tools the agent has. See below. | You, passed with `--tools-config`. |
| `~/.claude/projects/<escaped-cwd>/<id>.jsonl` | The transcript, one file per session. | The agent. |
| `~/.claude/projects/<escaped-cwd>/<id>.name` | A session's `/rename`d name. | The agent. |
| `~/.claude/projects/<escaped-cwd>/<id>.gate` | The permission mode the session was last left in. | The agent. |
| `~/.claude/projects/<escaped-cwd>/<id>.model` | The model the session runs as. | The agent, via `/model`. |
| `~/.local/state/tab-atelier/agent-tasks/<escaped-cwd>.json` | The `Tasks` list for a directory. | The agent. |

`<escaped-cwd>` is the working directory with every character that is not a letter or digit
replaced by `-`: `/mnt/Dev/@wdes/mounch` becomes `-mnt-Dev--wdes-mounch`. That is how one
directory's sessions and task list stay separate from another's.

Each of the sidecars beside a transcript is a decision about *that session* rather than
about the program — which is why they are files and not flags. A flag would have to be
passed again on every resume.

## Giving it an identity

By default the agent sends Claude Code's own identity line as its system prompt, and drops
it when the relay reports a model that is not Anthropic's — because claiming to be
something it is not, on a model that is not, is worse than saying nothing.

To say something else, write a markdown file. The text replaces the whole system prompt:

    ---
    AllowedTools: Read, FileTree
    ---
    You are Tabby, a very skilled and technical PHP and VueJS master.

Put it at `~/.config/tab-atelier/catbus-agent/identity.md`, and it is used automatically.
Or name one for a single run:

    catbus-agent --identity-file .claude/TABBY.md
    catbus-agent --identity "You are a terse filing clerk."

Both are also environment variables: `CATBUS_IDENTITY_FILE` and `CATBUS_IDENTITY`.

The front matter is optional, and it decides which tools the file's author is willing to
have. `AllowedTools: Read, FileTree` means those two and no others — it narrows whatever the
launch configured and can never widen it, so a prompt file cannot grant itself a tool you
withheld. A name that is not available is an error rather than a silent drop, because a
permission list with a typo in it should say so.

A file whose body is blank but whose front matter names tools still limits them; that is how
you say "no identity, but only these tools".

The rendering instructions are appended after your text and are not yours to replace: they
describe the terminal, not the model, and a model told to write markdown into a terminal
that renders it needs to know that whoever it thinks it is.

## Choosing the tools

`--tools-config` takes either a keyword or a path:

| Value | Tools |
|---|---|
| `minimal` | `Read`, `Write`, `FileTree` — no shell. The default for a `Spawn`ed sub-agent. |
| *(omitted)* | Everything the build has. |
| a path | A JSON file, below. |

```json
{
  "disable": ["Bash"],
  "add": [
    {
      "name": "GitBisect",
      "description": "List the commits between a good and a bad revision.",
      "argv": ["git", "log", "--oneline", "--ancestry-path", "{good}..{bad}"],
      "schema": {
        "type": "object",
        "properties": {
          "good": { "type": "string" },
          "bad": { "type": "string" }
        },
        "required": ["good", "bad"]
      },
      "timeout_secs": 30,
      "judged": false
    }
  ]
}
```

The command is **`argv`**, one array — there is no `command` plus `args`. Each element becomes
exactly one argument, so nothing is shell-parsed: a `good` of `v1.0; rm -rf /` is passed as a
single argument, not run. That is also why an unknown key is a mistake worth catching: an
extra `"command"` is ignored, and the config then fails on the missing `argv` rather than
doing what it looks like it does.

`disable` removes built-ins; `allow` keeps only what it lists. `disable` wins where they
overlap, so a tool in both is off.

`add` contributes a tool the agent runs itself, from `argv` with `{name}` placeholders filled
from the arguments the model passes. An *optional* placeholder must be a whole argument —
`["--max-count", "{max?}"]` — because dropping part of one would leave `--max-count` without
its value; `["--max-count={max?}"]` is refused at load, not at run. Required ones may be
embedded, as `{good}..{bad}` above.

`judged` defaults to **true**, so anything that executes is graded by auto mode unless the
config says otherwise. A tool that only reads should say `"judged": false`: the author states
a lower risk rather than the code assuming one.

**A custom tool may not take a built-in's name.** The agent refuses to start if one shadows
the other, on the grounds that a familiar name with different behaviour is worse than a
refusal. The built-ins are `Read`, `Write`, `Edit`, `FileTree`, `Bash`, `ListAgents`,
`Delegate`, `Spawn`, `Tasks`, `AskUserQuestion`, `PHPUnit`, `Composer`, `Bun`, `GitStatus`
and `GitCommit` — hence the name above, which cannot be `GitStatus`.

A working example is in the package at `/usr/share/doc/tab-atelier/tools.json`.

## Choosing the model

`/model <name>` switches for the rest of the session, and the choice is written beside the
transcript, so reopening continues with it. Run `catbus-agent --help` to see the provider
flags: `--model` and `--api-url` for an Anthropic-compatible endpoint, `--openai-url` with
`--openai-model` for any OpenAI-compatible service, or `--infomaniak-*`.

On the relay path the model is chosen by the relay, not here — `/model` records what the
client asks for, and the relay decides what answers. The transcript records what *did*
answer, which is why the two can differ.

## Limiting the network

A tab can have its internet switched off. When it does, the agent is pointed at the app's own
relay on loopback, so it keeps working while nothing else can be reached. That is done for
you: `CATBUS_RELAY_URL` is injected into the tab's environment alongside the URL a `claude`
tab gets.

If you move the relay, both `ANTHROPIC_BASE_URL` and `CATBUS_RELAY_URL` need the new
address, or an internet-disabled tab's agent will not find it.

## Seeing what happened

The agent logs to stderr: `RUST_LOG=catbus_agent=debug catbus-agent` for detail, including
every tool the model called, what the gate decided, and what the relay reported serving. In
a tab, the log is below the visible floor, so `tab-atelier log` is the way to read it.

`--check` reports which runtime libraries are present and which directories are in use —
the first thing to run when it will not start.
