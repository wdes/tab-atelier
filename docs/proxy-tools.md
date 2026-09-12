<!-- SPDX-License-Identifier: MPL-2.0 -->

# Proxy-side tool policy

The proxy already rewrites `model` and can compact `messages[]`. `tools[]` is
the third part of the request and the one nobody reads. In a measured Claude
Code request it is **56,905 B — 19 % of a 306 KB body** — and **4,361 B of it
was ever called**.

This is the same chokepoint, the same per-account config, and the same
`serde_json::Value` the routing decision already paid to parse. What is
different is the failure mode. A compaction bug makes a turn expensive. A tool
policy bug makes a turn **fail**, on one provider and not the other, which is
the worst shape a proxy defect can take.

It is also the one pass that can *introduce* a `tools[]` where there was none:
`referenced ∪ pins` on a body with no `tools` key is pins-only. So the first
requirement below is not an optimisation.

## Two families of tool

A Claude Code request carries two kinds of tool, and they are not distinguished
anywhere in the schema. The work tools let the model act on the code:

| | |
|---|---|
| `Bash` `Read` `Edit` `Write` `NotebookEdit` `WebSearch` | 6 tools, **8,426 B** |

The harness tools let the model drive Claude Code itself — they exist because
of how the client is built, not because of what the user is doing:

| | |
|---|---|
| `Agent` `ListAgents` `SendMessage` `Workflow` `EnterPlanMode` `ExitPlanMode` `EnterWorktree` `ExitWorktree` `TaskStop` `TaskOutput` `ScheduleWakeup` `CronCreate` `CronList` `CronDelete` `AskUserQuestion` `ReportFindings` `Skill` | 17 tools, **48,479 B** |

**85 % of the tool payload describes the harness, not the work.** The giveaway
is the ratio of prose to schema — a work tool describes a capability and needs
parameters to say how; a harness tool describes a protocol the model is already
inside:

| tool | description | schema | ratio |
|---|---|---|---|
| `EnterPlanMode` | 4,011 B | 119 B | **34:1** |
| `EnterWorktree` | 3,220 B | 687 B | 4.7:1 |
| `CronCreate` | 2,924 B | 958 B | 3:1 |
| `SendMessage` | 3,386 B | 1,291 B | 2.6:1 |
| `Bash` | 2,725 B | 1,038 B | 2.6:1 |
| `NotebookEdit` | 623 B | 943 B | 0.66:1 |

`EnterPlanMode` is four kilobytes of prose wrapped around one parameter. That
ratio is computable per request, so "is this a system tool" does not need a
hand-maintained list that goes stale every time the client ships — which it
does often: `Workflow`, `ScheduleWakeup` and `EnterWorktree` are all recent
arrivals, and each is ~3 KB.

In the measured session **21 of 23 tools were never called**; the history is
32 × `Bash` and 11 × `Read`. Even among the work tools, four of six were dead.

## `tools[]` is ahead of every breakpoint

The client's own `cache_control` markers, in render order:

```
tools       0 breakpoints
system[1]   breakpoint      "You are Claude Code, Anthropic's official CLI for Claude."
system[2]   breakpoint      the injected environment blob — cwd, git status, date
messages[76] breakpoint     the last message, so the whole history is inside it
```

There is no breakpoint in `tools[]`, so **every tool sits ahead of all of
them.** One byte changed in a tool description invalidates the entire 77,507
token prefix, where the same byte changed in the tail invalidates nothing. That
single fact decides the cost of each verb below, and it is why a tool policy
should be a deliberate act rather than something that happens to be on.

## The three verbs

| verb | safe when | buys | costs |
|---|---|---|---|
| **disable** | the name appears in no `tool_use` in `messages[]` | 52,544 B / 13,048 tok — **16.8 %** of the body | one cache re-warm |
| **whitelist** | always — it is a default policy | the same, and it fails *closed* for tools that do not exist yet | the same |
| **rewrite** | always, **including tools that are called** | prefix *stability*, and text that is true on this provider | nothing, when deterministic |

`rewrite` is the only one of the three that is safe on a tool the model is
mid-conversation with: it keeps the name and the parameter shape, so the
`tool_use` blocks already in the history still resolve.

**And the saving is smaller than the bytes suggest.** Cached input bills at
roughly a tenth of a miss, so 16.8 % off the body is 1.68 % off a full-price
request per turn. Measured against the one-time re-warm that editing the front
of the prefix costs:

| removed | tokens | per turn, at cache price | turns to break even |
|---|---|---|---|
| one tool, e.g. `NotebookEdit` (1,623 B) | ~390 | 0.05 % | **~1,700** |
| the 21 never-called tools (52,544 B) | 13,048 | 1.68 % | **~54** |

So the unit of decision is the **batch, not the tool**. Deleting one tool is
never worth the cache it costs; deleting twenty-one pays back over the course
of a long session and profits after it.

## Whitelist, not denylist

`docs/proxy.md` already argues this for forwarded headers, and the same
sentence applies verbatim: *an allowlist rather than a denylist, because a
denylist forgets.*

Claude Code ships new harness tools constantly. Every one of them is kilobytes
of prose about a protocol, and a denylist forwards each one by default until
somebody notices and adds it. An allowlist does not know about them either —
it just fails closed instead of open.

## `referenced ∪ pins`

Beyond `all` / `allow` / `none`, there is a mode that needs no list to
maintain, because the request states its own answer:

**keep the tools whose name appears in a `tool_use` block anywhere in
`messages[]`, plus an operator's pins.**

On the measured body that is `Bash` and `Read` — 4,361 B in, 52,544 B out.
Two properties make it safe:

- **It is monotone within a conversation.** History only grows, so a tool that
  is referenced stays referenced. The set can never oscillate and re-warm
  repeatedly, which is the failure a time- or size-based rule would have.
- **The scan is the same walk compaction already does**, and it is
  unambiguous — it reads names off `tool_use` blocks, not off anything the
  operator typed.

**Pins are not optional.** A conversation that has not searched the web yet
cannot start: a tool absent from `tools[]` can never be called, so it can never
become referenced. `referenced` alone freezes the tool set at whatever turn one
had, which is right for an established session and wrong for a fresh one. The
pins are short — the few tools the model should be able to reach for at any
point.

## The rewrite case: WebSearch

One tool in this payload is not merely large, it is **mutable**, and it is in
the worst place for it. Its description reads:

```
Search the web. Returns result blocks with titles and URLs. US-only.

- The current month is September 2026 — use this when searching for recent information.
- `allowed_domains` / `blocked_domains` filter results.
- After answering from results, end with a "Sources:" list of the URLs you used
  as markdown links.
```

Three sentences, three different problems:

1. **The month is a duplicate.** Claude Code already injects the live date into
   `system[2]`, as `# currentDate / Today's date is 2026-09-10`. The model
   loses nothing if the tool description drops it — it is the same fact, one
   section earlier, and this copy is **ahead of every cache breakpoint while
   that one is not.** On the first request of each month the whole prefix is
   invalidated for every session, on every machine, to deliver information the
   request already carries.
2. **"US-only" becomes false on a reroute.** It describes Anthropic's search
   backend and travels unchanged to a provider that is not Anthropic. It does
   not break anything; it is the proxy putting provider-specific text into a
   provider-neutral request.
3. **The `Sources:` line is a rendering convention**, not a property of the
   tool — the same category as `Agent` and `ListAgents` describing the harness
   to the model running inside it.

The rewrite drops the date and the provider claim and keeps the instruction:

```
Search the web. Returns result blocks with titles and URLs.

- `allowed_domains` / `blocked_domains` filter results.
- Prefer recent sources when the question is time-sensitive; put the period in
  the query rather than assuming it.
- After answering from results, end with a "Sources:" list of the URLs you used
  as markdown links.
```

This is the verb worth having even where the others are not. `disable` trades
bytes for a cache re-warm. `rewrite` makes the prefix *stop changing*, which is
the larger prize — and on the Anthropic hop, where the cache is everything,
taking volatility out of `tools[]` is the only one of the three that can pay.

## What must never change

Verified on the measured body, and the first of these is not a heuristic:

- **A `tool_use` name that survives in the history must resolve in `tools[]`.**
  43 `tool_use` blocks in the history, naming `Bash` and `Read`. Dropping either
  from `tools[]` while keeping the history is a request the API can refuse.
- **The scan runs against `messages[]`, never against the operator's list.**
  The policy is a preference; the history is the fact.
- **A body with no `tools` key is not a candidate for the pass at all.** This
  is the classifier case, and it is the reason this policy is exempt from it:
  a body carrying no `tools[]` is not a request that chose its tools, and
  `referenced ∪ pins` would *add* them. Concretely, the auto-mode permission
  classifier is one such body — a judge written to emit a single parsed tag,
  which must not be handed a toolkit. See
  [`proxy-classifier.md`](proxy-classifier.md). Hard-coding the classifier
  check here would be the wrong shape; the rule is the missing key.

### The trap: the two providers disagree

Anthropic validates that a historical `tool_use` name appears in `tools[]`.
**DeepSeek's Anthropic-compatible endpoint does not** — probed directly: a body
with `Read` removed from `tools[]` while `Read` is called throughout the
history answered `200` with a tool call, `stop_reason: tool_use`.

That asymmetry is the danger. A scan that is too eager works perfectly on the
second provider and returns a 400 on the subscription — a defect that appears
only when routing moves, which is the one moment nobody is watching. The
default is `all`, and `disable` is a decision, not a fallback.

## The control

Per-**account**, beside `compact`, in `users.json` — the same reasoning that put
compaction there. The operator editing this is looking at a person; routing picks
the hop per request, so a policy filed under a provider silently comes to mean
something else the moment that provider stops being where the traffic goes.

```json
{
  "id": "u_…",
  "compact": "tools_thinking",
  "tools": {
    "mode": "referenced",
    "disable": ["WebSearch"],
    "allow": [],
    "add": [
      { "name": "ListAgents", "description": "…", "input_schema": { "type": "object" } }
    ]
  }
}
```

`mode` is `all` (default) | `referenced` | `allow` | `none`. `allow` carries its
own list in the same object. `disable` is checked after the mode, so it wins over
`all` — but never over the referenced-union rule below, which is what keeps a
disabled tool that the history already calls.

`add` is the override: a definition injected when the client did not send one.
Redefining a name the client *did* send is refused per request rather than
applied, because silently replacing the definition a session is mid-way through
is how a working tool turns into a mysteriously broken one. Names are matched
loosely (case-insensitively) so `Bash` and `bash` cannot both be live.

**`rewrite` is not implemented.** It was the third verb in the original design —
a fixed, named normalisation of a tool's description (strip a date, drop a
provider claim) rather than a free-text replacement, so that `providers.json`
stays readable and cannot smuggle prompt injection. It is the most valuable of
the three on the Anthropic hop, because a description that stops changing is a
prefix that stops changing, but it needs the named-normalisation catalogue and
its tests before it can safely exist. The `tools` object above has no such key;
adding one is a separate change.

The write is whole-object (`POST /api/users/<id>/tools`), not field-at-a-time:
`allow` means nothing apart from the mode that reads it, and `mode: allow` with
no list is `none` under another name. Partial writes are the only way to leave a
half-applied policy behind, so there are none.

### The two guards the field names invite you to get wrong

**`mode: none` is not a tool-removal switch.** It removes what the *client*
offers; it does not remove what `add` injects, and a body carrying no `tools[]`
at all still gets its additions. `disable: ["Bash"]` is how you remove one tool
by name. The two are easy to reach for interchangeably and only one of them is
right for any given intent.

**A policy for `add` with an empty `name` is refused at the API**, not silently
stored. An empty name matches nothing and can never match anything, so a policy
holding one is a typo that would be inert forever — the exact failure the
`Refusal` report exists to surface, caught one layer earlier where it can still
be reported to the person who made it.


## The honest limits

**An `add` definition is a promise the operator makes.** A name the client never
sends is a tool the model can call and nothing can answer — the proxy does not
execute tools, it only describes them. `add` is for restoring a tool the client
stopped shipping, or for a tool the client's own config cannot express; it is not
a way to give the model capabilities the harness will not perform.

**`referenced` cannot bootstrap.** It is the right default for a session in
progress and the wrong one at turn one, and there is no way to tell the two
apart from a single body. A session that starts under `referenced` gets only
`ToolSearch` (kept unconditionally) until the client sends real `tools[]`, which
Claude Code does on its first turn. On a client that does not, `referenced` is
`none` and the pins never arrive; `add` is what makes that configuration work.

**The volatility argument is measured, the prefix argument is inferred.** That
the client's own session served 77,312 of 77,507 tokens from cache on its 77th
turn is measured, and it establishes that this endpoint caches a real Claude
Code prefix. Whether editing `tools[]` at the front *keeps* the rest of the
prefix or discards it has not been probed — the numbers above assume the
pessimistic case, where it discards it. The test is cheap: send the baseline,
then the baseline with one appended message, and read the cache split. If the
prefix survives a tail change and not a tool change, the table stands. If it
survives both, the break-even above is conservative and the batch trim is
cheaper than stated.
