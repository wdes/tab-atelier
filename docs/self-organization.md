# A self-organising fleet

**Status: built.** `announce` / `bid` / `award` / `take` / `done` / `tasks`,
`gossip`, `backlog`, and the `/claims` + `/blackboard` routes are in the tree,
exercised by unit tests and by `scripts/self-org-sandbox.sh` (two real daemons
in isolated sandboxes).

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

**Claims deliberately do not travel.** A lease is host-local mutual exclusion,
and honouring a remote host's lease would mean trusting its clock. So a task
announced on one host can be taken on either, and a duplicate is discovered at
`done` time. That is the right trade when the unit of work is "spend some
tokens looking at a file" — not when it is "move money".

## Where the work comes from

`tab-atelier backlog`. A fleet waiting for a human to type tasks is remote
control, not self-organisation.

```
cargo llvm-cov --lcov --output-path target/lcov.info
tab-atelier backlog --limit 5 --audit-largest 2
```

It reads the coverage report, announces the worst-covered files ranked by
**absolute shortfall** (a 40%-covered 900-line file is worth more than a
10%-covered 30-line one), and rotates small audits over the largest files.

Two properties make it safe on a timer: task ids derive from the target
(`cov:src/api.rs`), so a repeat sweep announces nothing while the task is open;
and finished tasks stay off the board for `--cooldown` days, so the fleet moves
on instead of re-auditing yesterday's file forever. It reads a report rather
than shelling out to cargo, because generating one is a multi-minute build and
that is the caller's decision, not a side effect of looking for work.

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
