# Teamwork — making Claude tabs work together

Every tab can shell out to the local API (the CLI discovers the token the same
way `brain` does), so the `claude` sessions can coordinate directly — no catbus
agents involved. The verbs live in `src/cli/team.rs` (`peers`, `note`/`notes`,
`handoff`) and `src/cli/delegate.rs` (`dispatch`).

## Send a prompt to another agent — `dispatch`

```
tab-atelier dispatch --to <tab> "<prompt>"           # fire-and-forget (tell)
tab-atelier dispatch --to <tab> --wait "<prompt>"    # wait until idle, print its reply (ask)
tab-atelier dispatch --new --name build "<prompt>"   # spin up a fresh agent tab
```

`<tab>` is a name, index, or UUID. `--wait` polls the target's screen until it's
been unchanged for `--quiet` seconds (default 8) — the agent went idle — then
prints it. See `cli::delegate` for `--timeout`.

## See who's around — `peers`

```
tab-atelier peers          # Claude tabs only
tab-atelier peers --all     # every tab
```

Lists `[idx] name · state · cwd — context`, so you can pick a collaborator or
wait for one (`state` back to `idle`/`waiting`) before reading its output.

## Broadcast — `note` / `notes`

An append-only blackboard at `<state>/tab-atelier/blackboard.jsonl` every tab
reads. Good for fan-out ("schema changed, endpoints moved") rather than
point-to-point.

```
tab-atelier note --topic schema --from api "users.email is now NOT NULL"
tab-atelier notes --topic schema           # read a channel
tab-atelier notes --since 42               # only entries after index 42 (poll incrementally)
```

## Hand off a file — `handoff`

```
tab-atelier handoff ./report.md db-expert
```

Copies the file into the target tab's `inbox/` (the same place web uploads land),
so its agent can pick it up. Target resolved by name/index/UUID; an ambiguous
name errors with the candidate indexes.

## Safety

- Only `dispatch` to a tab that's at a prompt (`peers` shows `idle`/`waiting`),
  never mid-turn.
- Never `--resume`/`--continue` another tab's session — it rotates/strips the
  session id.
- A locked tab refuses input.

## Telling every Claude these exist

Drop this into `~/.claude/CLAUDE.md` (or a project `CLAUDE.md`) so each session
reaches for the verbs on its own:

```markdown
# Working with sibling tabs

You run inside a tab-atelier tab alongside other `claude` sessions. Coordinate
with them via the `tab-atelier` CLI (already on PATH, token auto-discovered):

- `tab-atelier peers` — list sibling tabs (name, state, cwd, current task).
- `tab-atelier dispatch --to <tab> --wait "<question>"` — ask another agent and
  get its answer back. Drop `--wait` to just hand off work.
- `tab-atelier note --topic <t> "<msg>"` / `tab-atelier notes --topic <t>` —
  shared blackboard for broadcasts.
- `tab-atelier handoff <file> <tab>` — put a file in a teammate's inbox/.

Only message a tab that `peers` shows as idle/waiting. Never resume or continue
another tab's Claude session.
```
