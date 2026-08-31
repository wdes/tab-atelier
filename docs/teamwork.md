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

## Label a tab — `set-meta`

A small key/value map that lives on the tab, survives a restart, and comes back
on `tabs --json` — so an agent can re-read who it is after a compaction, and a
supervisor can see the whole fleet's roles in one call.

```
tab-atelier set-meta role reviewer          # on my own tab
tab-atelier set-meta project kalpin-back
tab-atelier set-meta role --clear
tab-atelier set-meta --tab 3 role builder   # on someone else's
tab-atelier tabs --json | jq '.tabs[] | {name, meta}'
```

**We assign no meaning to any key.** An orchestration layer on top brings its
own vocabulary (`role`, `phase`, `objective`, whatever it needs) without the tab
model growing a field per idea. Keys are `[a-z0-9_-]`, up to 16 per tab; values
up to 256 chars. Unlike `env set --tab`, nothing here reaches the PTY, and
unlike `set-context` (one free-text line, shown as the tab's tooltip) it's
structured and multi-valued.

## Run a daemon as a tab — `--daemon`

`⛑ brain` is a tab-atelier subcommand running as its own tab. Anything else
shaped like it — a queue drainer, a context watcher — announces itself the same
way, and the daemon then comes back on its own after a restart:

```
tab-atelier set-status thinking --kind mywatcher --daemon
```

Restore relaunches `tab-atelier <kind>` for a flagged tab instead of dropping to
a shell. The kind must be a plain lowercase verb; the flag is what elects the
tab, so a stray `--kind` alone never turns into a command line.

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
