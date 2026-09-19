# Working list — catbus-agent, 2026-09-19

Requested by the operator, in their order of mention. Each item gets its own
commit, verified with the gate (`fmt` / `clippy --all-targets` / `test`) before it
lands. Nothing is pushed: pushes need the Nitrokey.

| # | item | state |
|---|------|-------|
| T1 | `Tasks` tool for catbus-agent — a task list with actions on the items | **done** |
| T2 | `/auto` does not work | next |
| T3 | Right-click "Catbus" should not be offered when the tab already has an agent | todo |
| T4 | Thinking traces are not surfaced | todo |
| T5 | Approval logging — nothing records what the gate allowed | todo |
| T6 | Model on launch — pick a model per tab | todo |
| T7 | Identity file (`---` front matter, `AllowedTools:`) — approved earlier | todo |

Already landed this session, for reference: the empty-content 400, the resume
history bug, `is_error` round-trip, `end_turn` + `tool_use`, the `Spawn` tool,
the boot collision guard, the local relay for internet-disabled tabs, the slash
command table, and the GUI status line.
