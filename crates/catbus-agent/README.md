# catbus-agent

A small standalone agent loop: six tools, its own transcript, talking to the
relay. `catbus` is the bus between tabs; this is the agent on it.

## Permission modes

The agent has three, and they are exclusive — `/plan`, `/auto` and `/open` are
not independent switches:

| mode | what it does |
|---|---|
| `/plan` | write, edit and bash refuse; the agent proposes instead |
| `/auto` | each write, edit and bash is graded before it runs (see below) |
| `/noplan`, `/noauto` | nothing is checked |

Reading is never blocked in any mode. The monitor prompt auto mode uses has an
explicit read-only exception, and an agent that had to pass a safety check to
read a file would be slower than plan-mode while guarding nothing.

`/plan` and `/noplan` keep working, so an existing habit is not broken. The
socket keeps the old `{"kind":"set_plan_mode","on":true}` request too, and a
client that only knows about it simply never sees the third state — which is the
right degradation, because it never asked for it.

## Auto mode

A second, much smaller request grades the action and answers with a severity on
0–100. **Above 50 blocks.** The number is the whole interface: no reason string,
no rule identifier. A blocked action comes back as a tool error naming the
severity, so the model can see the refusal and propose something else rather
than repeating the call.

It judges the whole transcript, not a recent window, because the sharpest risk
is spread across turns: a file read early and a write derived from it much later
looks innocent in a window showing only the write.

**It fails closed.** An unreachable judge, a timeout, and an unparseable answer
all refuse the action. A judge that can be defeated by making it fail is not a
judge.

### What it costs

Roughly **$0.008 per judged action** on a 50k-token session and **$0.075** on a
500k one at Flash rates. The judge's own prompt is fixed and caches after the
first call; the transcript does not, and the transcript is what you pay for.
Only write-capable actions are judged, which keeps the count low.

`--judge-model` (or `CATBUS_JUDGE_MODEL`) picks the model, defaulting to a cheap
Flash-class one. It is deliberately **not** the agent's own model: grading a
proposed command is classification rather than reasoning — the captured request
the design is copied from uses 64 output tokens and disables thinking — and
grading every write with a heavy model costs an order of magnitude more than the
judgement is worth.

### The prompt

The built-in monitor prompt is a concise implementation of the published
contract: the 0–100 scale with 50 as the boundary, a hard-block list that
outranks context (prompt injection, credential harvesting, destructive writes,
exfiltration), and the read-only exception.

Anthropic's own prompt is 128,810 characters and travels inside every classified
request rather than being published. It is deliberately **not** in this
repository — it is not ours to redistribute, and a copy here would rot while the
real one is what actually decides. If you want exact fidelity, install it
locally and point the agent at it:

```sh
catbus-agent --monitor-prompt /path/to/your/monitor-prompt.txt
```

A configured path that cannot be read is a startup error rather than a fallback.
Judging with a prompt you did not choose would make the setting appear to work
while changing nothing.

## Tools

Six built in: `Read`, `Write`, `Edit`, `Bash`, `ListAgents`, `Delegate`. They can
be removed and added to without a rebuild:

```json
{
  "disable": ["Bash"],
  "add": [
    {
      "name": "GitStatus",
      "description": "Show the working tree status.",
      "schema": { "type": "object", "properties": {}, "required": [] },
      "argv": ["git", "status", "--short"],
      "timeout_secs": 10,
      "judged": false
    },
    {
      "name": "CargoTest",
      "description": "Run the test suite, optionally filtered by name.",
      "schema": {
        "type": "object",
        "properties": { "filter": { "type": "string" } },
        "required": []
      },
      "argv": ["cargo", "test", "{filter?}"],
      "timeout_secs": 300,
      "judged": true
    }
  ]
}
```

```sh
catbus-agent --tools-config ./tools.json
```

A working set is committed at `examples/tools.json` — `git status`, `git log`,
`cargo test` and `cargo check`, with `Bash` removed.

`tests/relay.rs::configured_tools_execute_for_real` runs this for real: it starts
the actual binary with a config file, has the (mocked) model call the tools, and
asserts the results are the real output of real `git` and `cargo` subprocesses.
Only the model is simulated.

- **`disable`** / **`allow`** filter the built-in set. `"disable": ["Bash"]`
  removes shell access entirely; `allow` keeps only what it lists.
- **`add`** contributes a tool the agent runs itself.

  A custom tool may not take a built-in's name. The agent refuses to start if one
  shadows the other, on the grounds that a familiar name with different behaviour
  is worse than a refusal. The built-in names are `Read`, `Write`, `Edit`,
  `FileTree`, `Bash`, `ListAgents`, `Delegate`, `Spawn`, `Tasks`, `PHPUnit`,
  `Composer`, `Bun`, `GitStatus` and `GitCommit` — so the example's git tool is
  `GitBisect`, and a config `add`ing a `GitStatus` of its own will not start.
  Rename it, or add the built-in to `disable`.
- **`judged`** defaults to `true`. Auto mode grades anything that executes
  unless the tool says otherwise, so the safe default is the default and a
  read-only tool states its own lower risk.
- A duplicate name, an empty `argv`, or a custom tool shadowing a live built-in
  is a **startup failure**. A tool set that silently differs from what you wrote
  would tell the model it has a tool that behaves otherwise.

**No shell, ever.** `argv` is executed directly, with `{param}` placeholders
substituted as whole arguments. `["git", "log", "-n", "{count}"]` with
`count = "5; rm -rf /"` runs `git log -n '5; rm -rf /'` — one argument, no
expansion, nothing to escape. Nothing is ever handed to `sh -c`.

A placeholder written `{param?}` is **optional**: when the argument is absent the
whole argv entry is dropped, so `["cargo", "test", "{filter?}"]` runs plain
`cargo test` when no filter was given. An optional placeholder must be a whole
argument — `-n{count?}` is refused at startup, because dropping it would lose the
flag and keeping it would pass a bare `-n`.

The set is resolved **once, at startup**. Changing the file needs a restart, and
that is a cache decision as much as a design one: the tool array is the first
element of an Anthropic body, so an array that moved mid-session would
invalidate every cache breakpoint behind it.

## Prompt caching

Requests carry `cache_control` breakpoints on both system blocks and the last
real turn — the placement measured from real Claude Code traffic, which marks
its system blocks and its last message and does not mark the tool array at all
(a message breakpoint already covers everything before it).

Two things follow from that, and both are easy to get wrong:

- **Nothing session-specific goes in `system`.** The working directory and the
  permission mode used to be a system block, *before* the static instructions,
  so toggling `/plan` invalidated the static block and every message after it —
  the whole conversation re-bought to change twenty bytes. They are now a
  trailing turn, appended only when they change.
- **The breakpoint goes before that turn, not on it.** Marking the last message
  when the last message is a rebuilt env turn would cache the stable history
  behind a block that changes every request.

## Configuration

| flag | env | meaning |
|---|---|---|
| `--relay-url` | `CATBUS_RELAY_URL` | the relay endpoint |
| `--relay-token` | `CATBUS_RELAY_TOKEN` | its credential |
| `--judge-model` | `CATBUS_JUDGE_MODEL` | what auto mode grades with |
| `--monitor-prompt` | `CATBUS_MONITOR_PROMPT` | a file to use instead of the built-in prompt |
| `--tools-config` | `CATBUS_TOOLS_CONFIG` | the tool set file above |

## Rate limits

A 429, a 5xx (other than 501), or a 408 is retried up to four attempts, waiting
`Retry-After` when the server sends one and backing off exponentially otherwise.
The wait is capped at 30 seconds: a server asking for an hour gets a visible
error rather than an agent that appears to work for an hour.

Deterministic failures — 400, 401, 403, 404, 422, and 501 — are returned
immediately. Retrying them is five times the latency for the same answer, and
the user cannot tell that apart from the agent being slow. 501 is the interesting
one: it is a 5xx, but "not implemented" is a statement about the request.
