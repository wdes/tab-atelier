# Configuring the agent

Where its files live, how to give it a different identity, and how to choose which tools it
has. Written for someone with the package installed and no checkout.

## Files

| Path | What it is | Set by |
|---|---|---|
| `~/.config/tab-atelier/preferences.json` | The relay endpoint. The agent reads this for its URL and token, so a tab needs no flags. | `tab-atelier` writes it; `/etc/tab-atelier/preferences.json` is the fallback for a fresh install. |
| `~/.config/tab-atelier/catbus-agent/identity.md` | The system prompt. See below. | You. |
| `<cwd>/.catbus/identity.md` | The system prompt for one directory. See below. | You. Ignored by git, not committed. |
| `~/.config/tab-atelier/catbus-agent/tools.json` | Which tools the agent has. See below. | You, passed with `--tools-config`. |
| `<cwd>/.catbus/tools.toml` | Tools for one directory, found without being named. See below. | You. Ignored by git, not committed. |
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

### Per-directory identities

A working directory can carry its own identity at `<cwd>/.catbus/identity.md`. It is the more
specific statement, so it wins over the one above, and it is how a project says who its agent
is without that answer depending on which machine the tab runs on. `.catbus/` is worth
ignoring rather than committing: the front matter is where `AllowedHosts` and
`AllowedJumpHosts` live, and a host limit describes where the work runs rather than the work.

The search is `--identity`, then `--identity-file`, then `<cwd>/.catbus/identity.md`, then the
one above. The first two are *named*, so a name that does not resolve is an error; the last two
are *found*, so one that is absent is not a statement and the search carries on. A found file
that exists but has a blank body is a statement — it silences the identity, and being nearer
it silences the ones behind it too, which is how a directory turns off a prompt it inherits.

### Limiting which hosts SSH may reach

`AllowedHosts` does the same job for the `SSH` tool — where it may connect, rather than what may
be used:

    ---
    AllowedTools: Read, FileTree, SSH
    AllowedHosts: dc1.example.org, *.staging.example.org
    ---
    You are Tabby, a PHP and VueJS master.

An entry is either an exact host or a leading `*.` for a domain and its subdomains. `*.example.org`
matches `api.example.org` and not `example.org` itself, deliberately: a wildcard that also covered
the bare domain would be wider than it reads. There is no CIDR syntax — `10.0.0.0/8` is not a
hostname and is refused — so a range has to be written as the names or domains in it.

Leaving `AllowedHosts` out means no opinion — every host the session can reach is reachable.
Writing it with no names at all (`AllowedHosts:`) means the opposite: no host is allowed.

The check happens before anything runs, so a host that is not listed never reaches a connection
attempt. It is the operator's limit rather than a suggestion, and the refusal says so and names the
list, so an agent reports it rather than trying to work around it.

A port is not part of the policy. `AllowedHosts` is about where, so `dc1.example.org` also
permits `dc1.example.org:2222`.

### Routing through a jump host

A second key grants hopping through another machine:

    AllowedJumpHosts: bastion.example.org, *.bastions.example.org

**The two keys disagree about being absent, deliberately.** No `AllowedHosts` means no destination
was restricted; no `AllowedJumpHosts` means **no jump host is permitted at all**, and the refusal
names the line to add. Destinations are a range, so silence has to mean unrestricted — a tool whose
job is connecting somewhere cannot read silence as "nowhere". A jump host is a capability: a machine
the connection passes *through* rather than another place to connect, so allowing destinations does
not imply allowing a route to them. Capabilities are granted; ranges are limited.

That is the safe direction. The permissive mistake would let an agent route a connection through a
machine the destination list never mentioned.

A jump host is checked against `AllowedJumpHosts` alone, not against `AllowedHosts` — it is a route,
not a destination. `keyscan` cannot use one (it connects directly), and says so when asked.

The rendering instructions are appended after your text and are not yours to replace: they
describe the terminal, not the model, and a model told to write markdown into a terminal
that renders it needs to know that whoever it thinks it is.

## Choosing the tools

`--tools-config` takes either a keyword or a path:

| Value | Tools |
|---|---|
| `minimal` | `Read`, `Write`, `FileTree` — no shell. The default for a `Spawn`ed sub-agent. |
| *(omitted)* | Everything the build has, plus the working directory's own file if it has one. |
| a path | A config file, below. `.toml` is read as TOML and `.json` as JSON; a path with no extension is tried as JSON and then TOML. |

A config file's shape, in TOML:

```toml
disable = ["Bash"]

[[add]]
name = "GitBisect"
description = "List the commits between a good and a bad revision."
argv = ["git", "log", "--oneline", "--ancestry-path", "{good}..{bad}"]
judged = false
timeout_secs = 30
schema = { type = "object", properties = { good = { type = "string" }, bad = { type = "string" } }, required = ["good", "bad"] }
```

The same body in JSON is documented below; the fields are identical, and either format is
read by the same parser once it has been deserialised.

### Per-directory tools

A working directory can carry its own tools at `<cwd>/.catbus/tools.toml`, beside the identity
file. Like the identity it is **found rather than named**: a project describes what its own work
needs, and a tab started in it should not need a second flag. The file you pass with
`--tools-config` still applies, and the two are layered:

- **`add`** from both, the launcher's first. A name both files define is a refusal that names
  both, not a silent override — and a project tool may not take a built-in's name.
- **`disable`** is the union. Either file can withhold a built-in; neither can un-withhold.
- **`allow`** may appear only in the launcher's file. A project that sets one is refused, because
  two whitelists would make the live set an intersection the operator cannot see in one place.
  `AllowedTools` in the identity file is the instrument for narrowing, and it applies to custom
  tools too — so a project tool must *also* be named in `AllowedTools` when the identity has one.
- **`phpunit_disable_functions`** is the project's when it sets one, since the project is the one
  whose tests are being run.

`minimal` is exempt: it is a deliberate lockdown, so a directory's file is not read at all.

`.catbus/` is worth ignoring rather than committing — like the identity, the file is a statement
about the machine and the operator rather than about the work.

### The example in JSON

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

`disable` removes built-ins; `allow` keeps only what it lists. `disable` wins where they
overlap, so a tool in both is off.

Write the top-level keys (`disable`, `allow`, `phpunit_disable_functions`) **before** the first
`[[add]]` table. In TOML a key written after a table belongs to that table, so a top-level
setting placed at the bottom of the file is absorbed by the last tool and ignored — silently,
since an unknown field on a tool is not an error. The JSON form has no such rule.

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
`Delegate`, `Spawn`, `Tasks`, `AskUserQuestion`, `PHPUnit`, `Composer`, `Bun`, `SSH`
and `Git` — hence the name above, which cannot be `Git`.

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

The agent logs at `info` and up. **Where** depends on the mode, because the TUI owns the
terminal: a log line written to stderr lands wherever the cursor happens to be, and the TUI
will not repaint it — so it reads as words spliced into your prompt.

- **With the TUI** (the default): `$XDG_STATE_HOME/tab-atelier/catbus-agent.log`, else
  `~/.local/state/tab-atelier/catbus-agent.log`. Truncated at each start, so it always
  describes the run that just happened. This is the log to read when the TUI itself is
  misbehaving.
- **`--no-tui`**: stderr, since nothing is drawing on those tabs and a log in place is more
  useful than a file.

`RUST_LOG` overrides the level either way — `RUST_LOG=catbus_agent=debug catbus-agent` for
every tool the model called, what the gate decided, and what the relay reported serving. If
the state directory is not writable the agent says so once and falls back to stderr at
`warn`, rather than silently keeping a log nobody can find.

`--check` reports which runtime libraries are present and which directories are in use —
the first thing to run when it will not start.
