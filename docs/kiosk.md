# The Kiosk — how to use it

The Kiosk is a web page served by `tab-atelier-kiosk`, at:

    http://127.0.0.1:8282/kiosk?token=<token>

The token is required. Get it with `tab-atelier token` (the **master** token —
a tab's share link has a *different* token that is refused here; see
"Wrong token" below). It has three tabs, and each one is a different thing:

| Tab | What it is for | Where the content lives |
| --- | --- | --- |
| **Décisions à prendre** | what needs a human ruling | the daemon (`/decisions`) |
| **Rapports** | reports written by workers | `<tab.cwd>/outbox/*.md` |
| **Intention** | circling a new intention with a worker | `~/Dev/intentions/*.md` |

This page is about using it. To run it, see the README beside it.

---

## Décisions — the verb agents get wrong

The `decision` verb has **no `--help` for its subcommands**: `tab-atelier decision
--help` prints the *global* options, which says nothing about the five commands.
Run `tab-atelier decision` with no argument instead — that is what prints the
usage:

```
tab-atelier decision push  --id <id> --project <p> --title <t> [--summary <s>] [--why <w>] [--reco <r>] [--effort <e>] [--detail <body>] [--files a,b] [--from <pusher>]
tab-atelier decision compose --id <id> --title <t> [--summary <s>] [--from <x>] [--enjeux <e>] [--options "A: …"]… [--reco <r>] [--effort <e>] [--files <f>]… [--command <c>]… [--link <url>] [--reopen]
tab-atelier decision read   --id <id> [--by <who>]
tab-atelier decision tranch --id <id> --verdict <v> [--by <who>]
tab-atelier decision list   [--includeArchived]
```

Which one to use:

- **`push`** — the short form. Use it when you have decided what to ask.
- **`compose`** — the long form, with the *dossier*: `--enjeux` (what is at
  stake), repeated `--options "A: …"`, `--link`, `--command`. Use it when the
  decider will need the material to rule, not just the question.
- **`read`** — read one back before acting on it.
- **`tranch`** — rule on one, with `--verdict`. This is what closes a decision.
- **`list`** — what is pending. Add `--includeArchived` to see ruled ones too.

**`--id` is a slug you choose**, not a number the tool gives you. `--id conges-ete`
is fine. Reusing an existing id overwrites that decision, so pick one that says
what it is. An id that is not obvious from the title is an id nobody will find
again.

**Nothing is deleted.** A decision you regret is `tranch`ed with a verdict that
says so — that is the intended way to withdraw one, and it leaves the record
intact for whoever reads it next.

---

## Intention — the third tab

An intention is a `.md` file the PO and a worker circle together until it can be
planned. Three things move it forward:

1. **"Nouvelle intention"** asks for a title, a repo and one sentence of
   presentation, writes the file, and starts a diagnostic worker on it.
2. **The conversation** is the file. The worker's answers are appended to the
   same `.md` you are reading — nothing is kept in the browser, so a reload shows
   the same thing and a file edited by hand shows up as written. Only the
   worker's answers are shown; its tool calls are not in the transcript at all.
3. **"Marquer prête"** moves the file into `READY-intentions/`, where it becomes
   a pool a planner can draw from. **"Lancer"** starts an agent on a ready one.

A worker is found by the name `Intention <slug>`, so a tab you rename by hand
stops being found. The pane says **"sans worker"** when none is running for the
open intention — that means the file exists but nothing is answering, and the
message under the list says why when the launch failed.

---

## When it does not work

**Wrong token.** A tab's share token is refused by `/decisions` and by every
`/intent` route; they want the **master** token. The symptom is a page that loads
with "décisions indisponibles (HTTP 401)" — the page is fine, its API calls are
not. Run `tab-atelier token`.

**"no worker command configured".** The Kiosk was started without
`TAB_ATELIER_INTENT_CMD`, so it can create intentions but cannot start agents.
Not an error — an intention with no worker can still be written and promoted by
hand.

**"the daemon accepted the tab request but no tab appeared".** The daemon in
front is older than the Kiosk: `POST /tabs` there ignores `name`/`cmd`. The Kiosk
and the daemon have to be built from the same tree.

**A tab disappeared from the roster.** Restarting the daemon does this — the tabs
come back from the relay on the next tick, but one caught mid-restart may not.
Nothing to do about it after the fact; it is why `decision` state lives in files
rather than in a tab.

---

## For a worker: which tab is mine

- A **report** goes to `<tab.cwd>/outbox/` as a `.md`, then you **dispatch** the
  news to whoever asked for it. Writing the file is not delivering it — see the
  brief's own rule: a report exists only once dispatched.
- An **intention** is not a report. It is a question still being shaped, and it
  goes in the Intention tab, not in the reports list.
- A **decision** is what you push when you *have* the question and need someone
  else to rule on it.
