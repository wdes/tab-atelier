# Working list — catbus-agent, 2026-09-19

Requested by the operator, in their order of mention. Each item gets its own
commit, verified with the gate (`fmt` / `clippy --all-targets` / `test`) before it
lands. Nothing is pushed: pushes need the Nitrokey.

| # | item | state |
|---|------|-------|
| T1 | `Tasks` tool for catbus-agent — a task list with actions on the items | **done** |
| T2 | `/auto` does not work | **done** |
| T3 | Right-click "Catbus" should not be offered when the tab already has an agent | **done** |
| T4 | Thinking traces are not surfaced | **done** (transcript + REPL dimmed) |
| T8 | Migrate the agent's TUI to ratatui | **done** (spinner written in-tree) |
| T5 | Approval logging — nothing records what the gate allowed | **done** |
| T6 | Model on launch — pick a model per tab | **done** (`/model`, session state) |
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
- **T6** is done as session state rather than a flag: the model lives in a
  `.model` sidecar, `/model <name>` sets it, and startup reads it — so a resumed
  session continues with its model and nothing has to be passed again. It is *not*
  read from the transcript, which records what the relay **served**
  (`deepseek-flash` on every turn) rather than what the client asked for; that
  value is used only to decide whether to send the Claude identity line.
- **A third 400 shape, fixed separately:** a turn whose only block is a *full*
  thinking block. Emptiness was the wrong thing to look for the first time — the
  block is full, it just is not content, and an endpoint without thinking drops it
  in transit. Found by running the pipeline over the transcript that produced it:
  thinking-only turns at indices 3, 6, 9, 12, 15, 23, and the API named
  `messages.3` and `messages.6`.
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

## T8: the ratatui migration

`ratatui-spinner` is **not** usable, and the reason is worth keeping: the published
crate is a namespace reservation. Its README says it "intentionally exposes no public
API yet", and the whole crate is a 39-byte `lib.rs` that includes that README — 1389
bytes of tarball, no code. `github.com/ratatui/ratatui-spinner` 404s as well, and the
prototype the name probably referred to (`joshka/ratatui-spinner`, "design prototype
for time-aware spinner widgets") is gone too. So the spinner is written in
`tui/spinner.rs`, keeping the one idea the name should carry: a spinner that looks
identical after five seconds and after five minutes tells the operator nothing, so
the cadence slows and the glyphs coarsen as a turn drags on.

Three findings from the migration that should not have to be rediscovered:

- **A newline from anything but a terminal is Ctrl-J.** Driven from a pty, `/auto\n`
  arrives as `Char('a') Char('u') Char('t') Char('o') Char('j')` with CONTROL,
  because LF *is* Ctrl-J and is not `Enter`. Handling only `Enter` means the text is
  echoed correctly and the last key is swallowed, so the line never submits.
- **An inline viewport must not be resized.** Growing it from one row to two while a
  turn runs makes the terminal reflow the screen and reorders output already pushed
  above it. A fixed two-row viewport costs one blank line and removes the class.
- **`Some(ev) = events.recv()` in a `select!` never matches `None`,** so a dead input
  reader silently switched the REPL off while it went on redrawing. A REPL that draws
  but cannot be typed into is worse than one that stops.
