# A self-organising fleet

**Status: built.** `announce` / `bid` / `award` / `take` / `done` / `tasks` /
`wait` / `fleet`, `gossip`, `backlog`, and the `/claims`, `/blackboard` and
`/fleet` routes are in the tree, exercised by unit tests and by
`scripts/self-org-sandbox.sh` — 29 assertions against two real daemons in
isolated sandboxes.

The goal was a fleet that finds its own work — raising coverage, doing small
audits — without a scheduler telling each agent what to do, and without two
agents doing the same thing.

## Why not Paxos or Raft

This is the first question anyone asks, and the answer is that consensus
solves a problem tab-atelier does not have.

Consensus gets a group of peers to agree on replicated state **when none of
them owns it**. tab-atelier has an owner: one daemon per host, enforced by an
exclusive lock on the state directory, holding tab state behind a mutex. Two
agents racing for the same task are two HTTP requests arriving at one process.
Electing a leader that already exists buys nothing and costs a protocol.

Three sources shaped that call:

- **[Coordination avoidance](https://www.vldb.org/pvldb/vol8/p185-bailis.pdf)**
  (Bailis et al., VLDB 2015) gives the test — *invariant confluence*:
  coordinate only where an application invariant actually demands it. "Two
  agents must not hold one task" demands it. "Agent A observed that coverage
  moved" does not: observations merge.
- **[Chubby](https://www.usenix.org/conference/osdi-06/chubby-lock-service-loosely-coupled-distributed-systems)**
  (Burrows, OSDI '06). Google's answer to "our developers need consensus" was
  not a Paxos library, it was a **lock service** with a file-like API, because
  application developers do not build consensus in-app. Our daemon is already
  that shape; it was just missing the locks.
- **[Leases](https://web.eecs.umich.edu/~mosharaf/Readings/Leases.pdf)** (Gray
  & Cheriton, SOSP '89) is the primitive that was actually missing.

Consensus would earn its place if the claim registry had to survive a daemon's
death with state intact, or if hosts had to agree on a single ordering. Neither
is true, so it isn't built. The trigger to revisit is explicit: **when losing a
daemon must not lose in-flight claims**. Until then it would be a protocol
maintained for its own sake. (And "Paxos or Raft" is largely a false choice:
[they are equivalent](https://arxiv.org/pdf/2004.05074) in the part that
matters.)

## The three pieces

### 1. Leases, for the one thing that needs coordination

`src/claims.rs`, `POST /claims`.

```
tab-atelier take            # claims task:<id> for this agent
```

A lease, not a lock: **it expires**. Agent tabs die constantly — crashes,
compaction, `cgroup.kill`, an operator closing a tab — and a lock without
expiry in a fleet of ephemeral holders is a deadlock generator. A dead holder's
claim lapses and the work returns to the board.

Refusal returns **409 with the current holder**, not a bare no, so a losing
agent can pick different work instead of spinning on one key.

Every grant carries a strictly increasing **fence**. A holder that stalls past
its expiry can come back and try to finish work it no longer owns; comparing
fences tells the two apart. Renewal by the same holder is idempotent, so an
agent can refresh on a timer without tracking whether it still owns anything.

### 2. Contract Net, over the blackboard that already existed

`src/cli/tasks.rs` (the fold), `src/cli/work.rs` (the verbs).

Smith's **[contract net](https://dl.acm.org/doi/10.1109/TC.1980.1675516)**
(1980) allocates work by negotiation rather than by a scheduler: announce, bid,
award. The blackboard was already an append-only log every tab reads, so the
protocol needed no new transport — just typed entries and a fold.

The fold is the interesting part. Task state is **derived**, never stored, so
it cannot disagree with the log; and it resolves conflicts last-writer-wins by
`(ts, id)`, which makes the view a pure function of the entry *set* rather than
the sequence. That is what lets two hosts hold the same entries in different
orders and still agree about who won. `fold_tasks` is tested against rotation,
reversal and duplication for exactly this reason.

Which quadrant of
**[Gerkey & Matarić's taxonomy](https://journals.sagepub.com/doi/10.1177/0278364904045564)**
(2004) we are in matters: single-task agents, single-agent tasks, instantaneous
assignment (ST-SR-IA) — the case where a greedy claim is good enough and an
optimiser would be theatre.

### 3. Uncoordinated selection

Every agent sees the same board. If they all reach for the oldest open task,
all but one lose the lease and retry — the herd `brain` was rewritten to avoid.

`rank_tasks` orders candidates by `hash(task, agent)` — **rendezvous hashing**
(Thaler & Ravishankar) applied to work selection instead of cache placement. A
private ordering per agent means N agents facing N open tasks mostly spread out
on the *first* try, having exchanged no messages at all. Collisions still
happen; the lease settles them.

One implementation note worth keeping: the weight needs a real avalanche
finalizer. FNV-1a's trailing bytes barely move the high bits, so hashing
`"<task>\x01<agent>"` and sorting on the `u64` ranks by task and ignores the
agent — every agent then picks the same task first, which is precisely the
herd. The unit test caught that; splitmix64's finalizer fixes it.

## Across hosts: gossip, not replication

`tab-atelier gossip`, `GET`/`POST /blackboard`.

The blackboard is a **grow-only set** keyed by entry id, which makes it a
[CRDT](https://link.springer.com/chapter/10.1007/978-3-642-24550-3_29) (Shapiro
et al., 2011): merging is a union — idempotent, commutative, associative. Two
daemons exchanging batches in any order, any number of times, converge without
agreeing on anything first. A host that was offline catches up on its next
round and nobody had to track that it was away.

`gossip` does one push-pull round per peer (pull first, so the batch we send
already includes what they just taught us). It is safe on a timer: a round
against a converged peer reports `pulled 0 pushed 0` and costs one scan.

**Claims do not travel — the claimant does.** A lease table is not gossiped
(honouring a copy of a remote table would mean trusting its clock, and a
grow-only set cannot express a release anyway). Instead an agent taking a task
announced elsewhere claims it *at that host*. See the next section for why that
one piece is federated while everything else stays sovereign.

## Federation vs confederation

Worth being precise, because the words describe different amounts of ceded
sovereignty and tab-atelier deliberately sits between them.

A **confederation** is sovereign members cooperating by agreement. There is no
central authority; every member keeps full control of its own affairs, joins by
bilateral treaty, and can leave. BGP confederations work this way — each AS
keeps its own policy and merely exchanges routes.

A **federation** has a common authority for some matters. Members surrender a
defined slice of sovereignty to the union and are bound by decisions in that
slice, while keeping everything outside it. Federated identity is the usual
computing example: many services, one authority for *who you are*.

What gossip alone gives you is **confederal**. Each daemon is sovereign: it
owns its tabs, its policy, its resources, its lease table. It exchanges
knowledge voluntarily with peers it chose, by bilateral `remote add`. Nothing
can compel a member, and a member that stops gossiping simply drifts.

But knowledge alone is not enough, and the sandbox test proves it: two hosts
holding *identical* boards will still hand the same task to two agents, because
each consults its own lease table. Convergent knowledge, divergent decisions.

So the fleet federates exactly one thing: **arbitration of a task's lease**.

- Every task has a **home** — the host it was announced on, recorded in the
  entry that created it, so the fold picks the same one everywhere.
- Taking a task means claiming it **at its home host**, over the `remote`
  endpoint that already exists for sidecars. `take` routes the claim there and
  says so: *"leased task:cov:src/new.rs, from its home host build-box-3fee"*.
- Everything else stays sovereign. Tabs, resource limits, policy, which peers
  to talk to, and whether to run a backlog sweep at all are each host's own
  business. There is no federal government, only a registrar per name.

Membership is still bilateral — `remote add` is a treaty, not admission to a
union, and each side registers the other. A one-way registration is a legal
configuration; it just means the unregistered side can't reach the arbiter and
falls back to a local claim.

That fallback is the confederal escape hatch, and it is deliberate: **when the
home host is unreachable, the member claims locally and keeps working**, saying
so on stderr. A partition produces work, not a stall — at the price of a
possible duplicate discovered at `done` time. For "spend some tokens on a file"
that is the right trade. For anything where a duplicate is expensive, it would
not be, and that is when you would want the phase-3 consensus that this design
otherwise avoids.

## Waiting without long polls

`tab-atelier wait <task-id>… [--timeout <s>] [--any]`.

`dispatch --wait` holds a connection open and infers completion from a screen
that stopped changing. `wait` reports the outcome as an **exit code**:

```
0  all finished ok     2  usage error    4  unknown task
1  one or more failed  3  still running at the timeout
```

so it composes as a shell primitive, and `--timeout 0` makes the same verb a
status probe that returns immediately:

```sh
tab-atelier wait cov:src/api.rs && echo "and now the follow-up"
for t in $ids; do tab-atelier wait "$t" --timeout 300 & done; wait
```

It reads the board file rather than calling the API, so a hundred parallel
waiters are a hundred cheap reads, not a hundred held sockets. An unknown task
exits 4 rather than blocking forever — a typo should not look like slow work.

## Seeing it: the fleet graph

`GET /fleet`, or `tab-atelier fleet [--json]`.

Three things each know part of the answer to "who is working on what": the
board knows what was awarded, the lease registry knows what is actually held,
and the tab list knows who is alive. The route joins them into nodes
(`host` / `agent` / `task`) and edges (`runs_on`, `announced`, `bid`,
`works_on`, `home`, `peer`).

`works_on` carries `leased` and `expires_in_ms`, because the interesting state
is an award with **no live lease**: the agent said it was working and then
stopped holding the work — it died, or thought past its expiry. The text view
prints that as `NO LEASE`; a renderer should draw it in a colour that hurts.

Agents with no tab on this host are drawn too (`status: elsewhere`). In a
federated fleet most agents are somewhere else, and omitting them would render
work assigned to nobody.

## Where the work comes from

`tab-atelier backlog`. A fleet waiting for a human to type tasks is remote
control, not self-organisation.

It is **not** about coverage. A *source* is anything that prints candidate
tasks as `id<TAB>title`:

```sh
tab-atelier backlog --from './scripts/todo-tasks.sh'
tab-atelier backlog --from-file backlog.tsv
tab-atelier backlog --lcov target/lcov.info      # built-in coverage source
```

Sources compose — pass `--from` more than once and every candidate lands on one
board. What this owns is the part every source needs and none should
reimplement:

- **Idempotence.** A candidate already open, bidding or awarded is not
  announced again, so sources are free to emit their whole world every run;
  they don't have to remember what they said.
- **Cooling.** Finished work stays off the board for `--cooldown` days, so the
  fleet moves on instead of re-doing yesterday's task forever.
- **De-duplication.** Two sources proposing the same id announce it once.

That division is what makes it general: **the source decides what is worth
doing, the backlog decides whether it is worth saying.** A failing source is
reported and skipped rather than fatal — one broken generator must not stop the
others from finding work.

The built-in coverage source reads LCOV and ranks by **absolute shortfall** (a
40%-covered 900-line file beats a 10%-covered 30-line one), plus a rotation of
audits over the largest files. It reads a report rather than shelling out to
cargo, because generating one is a multi-minute build — the caller's decision,
not a side effect of looking for work.

## What the literature says will go wrong

**[MAST](https://arxiv.org/abs/2503.13657)** (Berkeley, NeurIPS 2025) analysed
150+ multi-agent traces into 14 failure modes: specification 41.8%, inter-agent
misalignment 36.9%, task verification 21.3% — and concluded better base models
will not fix them.

Two of their modes were live here before this work:

- *Unaware of termination conditions* — `dispatch --wait` infers completion
  from a screen that stopped changing for N seconds. A thinking agent looks
  finished. `done` is the explicit signal that replaces the guess.
- *Task verification* — nothing checks the work. **Still open.** `done --fail`
  distinguishes giving up from finishing, but nobody reviews a success. The
  obvious next slice is a `verify:` task announced automatically when a `cov:`
  task completes, taken by a *different* agent — the board already supports it,
  the policy doesn't exist yet.

## Trying it

```
cargo build --no-default-features --features headless
scripts/self-org-sandbox.sh
```

Two daemons in isolated `HOME`/XDG sandboxes on spare ports; nothing touches
your own tabs, state or blackboard. The agents are shell loops rather than real
Claude sessions — the point is to test the protocol without spending tokens or
depending on a model's behaviour. It asserts, against live daemons, that
concurrent takes never hand one task to two agents, that `done` frees the
lease, and that work announced on one host can be taken on the other and
travels back.

## Do we even need supervisors?

No, and that is the point — but "supervisor" hides three different jobs, and
only one of them needs a supervisor-shaped thing.

**Generating work.** Not a supervisor. `backlog` is a pure function of
measured state plus the board, it is idempotent, and any host may run it. Two
hosts running it concurrently produce the same announcements and the board
de-duplicates them. Nobody is in charge of it.

**Allocating work.** Not a supervisor, by construction. Rendezvous ranking plus
a lease allocates in one round trip with no coordinator; that is the whole
design. Contract net's *manager* role — announce, collect bids, award — is a
supervisor, and `take` deliberately skips it. `bid`/`award` remain for the case
where cost is knowable only to the bidder, which needs a decider; that is a
role a task can hand out, not a permanent office.

**Judging work.** This one is real, and it is the gap
[MAST](https://arxiv.org/abs/2503.13657) measures at 21% of multi-agent
failures. Somebody must check that a `done` means what it says. But that is
*work*, not authority: announce `verify:<task>` when a task completes, let a
different agent take it, and the same machinery applies. A verifier is a peer
with a task, not a boss.

What remains is a **watchdog**, which `brain` already is: it notices tabs that
have stopped and nudges them. It has no say over what anyone works on.

The honest summary: you need a *source*, an *arbiter of exclusion* (the home
host's lease table), and *verification as ordinary work*. None of those is a
supervisor. A human is still the one who decides what the fleet is for.

## Deliberately not built

- **Consensus.** See above; the trigger is claim survival across a daemon death.
- **Identity.** `--from` and the holder string are self-asserted. Fine within a
  host, where the daemon could bind them to the calling tab; not fine across
  hosts, where a peer can currently claim to be anyone. Worth fixing before
  gossiping with a machine you don't control.
- **Bidding as the allocation mechanism.** `bid`/`award` exist and fold, but
  `take` skips them: rendezvous ranking plus a lease allocates in one round
  trip, where an auction needs three and a manager. Bids are there for when a
  task genuinely needs comparison (cost known only to the bidder), not as the
  default path.
- **Automatic verification.** `done --fail` distinguishes giving up from
  finishing, but nothing checks a success. The board already supports the fix —
  announce `verify:<task>` on completion, require a different agent to take it —
  and it is the highest-value thing left.
- **Transitive membership.** A peer's peers stay invisible; every link is a
  bilateral `remote add`. Learning the fleet's shape by gossip would be easy and
  is deliberately not done: trust here is per-endpoint, and a transitively
  discovered member would be one nobody chose.
