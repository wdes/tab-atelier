# Working list — catbus-agent, 2026-09-19

Requested by the operator, in their order of mention. Each item gets its own
commit, verified with the gate (`fmt` / `clippy --all-targets` / `test`) before it
lands. Nothing is pushed: pushes need the Nitrokey.

| # | item | state |
|---|------|-------|
| T1 | `Tasks` tool for catbus-agent — a task list with actions on the items | **done** |
| T2 | `/auto` does not work | **done** |
| T3 | Right-click "Catbus" should not be offered when the tab already has an agent | **done** |
| T4 | Thinking traces are not surfaced | **needs a decision** |
| T5 | Approval logging — nothing records what the gate allowed | **done** |
| T6 | Model on launch — pick a model per tab | **needs a decision** |
| T7 | Identity file (`---` front matter, `AllowedTools:`) — approved earlier | **done** |

Notes, so the next session does not re-derive them:

- **T2 was not a broken judge.** Verified live: `/auto` judges correctly
  (`gate: auto checked Write: severity 10, allowed`). Two real faults made it look
  dead: the verdict that *allowed* was recorded nowhere (T5), and the mode did not
  survive the agent restart that a tab reopen causes — so a mode set yesterday was
  gone today. Both fixed.
- **T5** is the observable T2 needed. `Verdict::summary` now goes to the log and
  into the tool result on the allowed path, so a working gate is distinguishable
  from an absent one.
- **T4** — nothing renders or persists a thinking block. The question to settle
  first is where they should appear: the REPL, the transcript, the socket, or all
  three. They are already parsed (the empty-block fix depends on that) and already
  dropped before the transcript, so "surface them" is a real feature rather than a
  repair.
- **T6** is three flags, not one: `--model`/`--api-url` (Anthropic-compatible),
  `--openai-url`+`--openai-model`, `--infomaniak-*`. On the relay path the proxy
  picks the model, so it is none of those — decide which is meant before building.
- **T7** is complete, `AllowedTools` included: it narrows the launcher's tool set
  and can only narrow it, so a prompt file cannot re-add a tool `--tools-config`
  withheld.
- **ANSI is already conditional**, contrary to the note that prompted a look:
  `ansi::allow_escapes(sink_renders, TERM=dumb)` picks between `INSTRUCTIONS_TERMINAL`
  (invites SGR) and `INSTRUCTIONS_PLAIN` (forbids markdown *and* escapes, asks for
  plain prose), and `ansi::strip` scrubs escapes on the way out for sinks that
  render nothing — which is the `[36mRead[0m[0m` case its test documents. The
  residual weakness is scope: the *instruction* is per process while one session is
  read by several sinks (REPL, socket, phone), so a non-terminal sink gets stripped
  terminal-prose rather than plain prose. Worth fixing only if a phone actually
  reads it.

Already landed this session, for reference: the empty-content 400, the resume
history bug, `is_error` round-trip, `end_turn` + `tool_use`, the `Spawn` tool,
the boot collision guard, the local relay for internet-disabled tabs, the slash
command table, and the GUI status line.
