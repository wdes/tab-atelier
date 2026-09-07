# You are running in a tab-atelier tab

This file is injected into every Claude session that starts inside a tab, by
the `SessionStart` hook (`tab-atelier claude-hook session-start`). Keep it
short: it costs tokens in every session, on every tab, forever. Anything that
is not useful *within the first few turns* belongs in `docs/teamwork.md` or
`docs/self-organization.md`, which an agent can read on demand.

<!-- BRIEF-START -->
You are one tab among several. Other agents are running beside you, and you can
reach them from the shell.

**See and reach the others**
- `tab-atelier peers` — sibling tabs: index, name, state, cwd
- `tab-atelier dispatch --to <tab> "<prompt>"` — send work (`--wait` to block)
- `tab-atelier note --topic <t> "<msg>"` / `tab-atelier notes --topic <t>` — broadcast
- `tab-atelier handoff <file> <tab>` — drop a file in another tab's `inbox/`

**Shared work — no coordinator, no supervisor**
- `tab-atelier tasks` — the board
- `tab-atelier take` — lease the best open task *for you* (exit 3 = nothing free)
- `tab-atelier done <task> "<result>"` — finish it (`--fail "<why>"` if you gave up)
- `tab-atelier wait <task>` — block until it ends: exit 0 done, 1 failed, 3 running
- Never work on a task you did not `take`: the lease is what stops two agents
  duplicating each other. If `take` says a task is held, pick another.

**When you are finished**
- `tab-atelier close` — with no argument, closes YOUR tab. Do this when your
  work is done and reported, so the tab does not sit there reading "open".

**Etiquette**
- Only `dispatch` to a tab `peers` shows as `idle` or `waiting` — never mid-turn.
- Never `--resume`/`--continue` another tab's session; it strips the session id.
- Say who you are with `tab-atelier set-meta role <what-you-are-doing>`.

`<verb> --help` for details. The handbook is `self-organization.md` (the
model) and `fleet-playbook.md` (how to run a fleet) — in the repo's `docs/`,
or `/usr/share/doc/tab-atelier/` on a machine that only has the package.
<!-- BRIEF-END -->

## Per-project briefs

The built-in text above is the same everywhere. What differs per project goes
in a **brief directory**, as markdown files with front matter:

```
~/.config/tab-atelier/briefs/     per user
/etc/tab-atelier/briefs/          machine-wide
```

```markdown
---
baseDir: /mnt/clients/ABCD
---
Client ABCD: PHP 7.4, no `composer update` without asking. Deploys are manual —
never push to production yourself. Their staging DB is a copy from March.
```

Any session whose working directory is inside `baseDir` gets that text appended
to the brief. Repeat `baseDir:` for a project with several checkouts, or use
`always: true` for something that applies everywhere.

**Matches compose.** A note on `/mnt/clients` and a note on
`/mnt/clients/ABCD/api` both apply, ordered least-specific first, so the
narrower one has the last word — the position a reader weighs most. This is the
same cwd-prefix rule that decides per-project colours, so it should already feel
familiar.

Check what a directory produces before trusting it:

```
tab-atelier brief --cwd /mnt/clients/ABCD    # the exact text an agent gets
tab-atelier brief --list                     # which files matched, and from where
```

That command exists because the failure mode here is silent: a `baseDir` with a
typo produces no error, no warning, and an agent that simply never mentions what
you wrote.

Two limits worth knowing. Briefs are read **once, at session start**, from the
tab's cwd — an agent that later `cd`s elsewhere is not re-briefed, because
context can be added to a session but not withdrawn. And the total is capped at
6000 characters; over that, the least specific briefs are dropped whole (with a
note saying so) rather than truncated mid-sentence, which would read like a
corrupted instruction.

## Customising the built-in text

Drop your own `agent-brief.md` in the config directory
(`~/.config/tab-atelier/agent-brief.md`, or `/etc/tab-atelier/` for the whole
machine) and it replaces the built-in text verbatim. The hook reads that file
if it exists, so you can change what every agent is told without rebuilding.

## Why a hook and not a skill

Three mechanisms could carry this, and they are not equivalent:

| | loaded | scope | cost |
|---|---|---|---|
| **SessionStart hook** | always | every session on the machine | tokens every session |
| **Skill** | when the model decides it is relevant | per user (`~/.claude/skills/`) | ~free until used |
| **CLAUDE.md** | always | per user or per project | tokens every session, manual install |

The hook wins here for one reason: **it is already installed**. The deb ships
`/etc/claude-code/managed-settings.json` pointing every `claude` on the box at
`tab-atelier claude-hook`, so there is no per-user setup to forget, and it can
be conditional — the brief is only injected when `_TAB_ID` is set, so a Claude
running *outside* a tab is told nothing about verbs it cannot use.

A skill would be the better home for the long version (progressive disclosure:
free until the model reaches for it), but skills load only from
`~/.claude/skills/` and `.claude/skills/` — there is no system-wide directory,
so shipping one in a deb would mean writing into each user's home. That is why
the brief is here and the depth is in `docs/`.
