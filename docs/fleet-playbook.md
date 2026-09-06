# Playbook: driving a fleet from a tab

Hand this file to a Claude tab and it can run a fleet of worker agents. It is
written for the **driver** — the tab that puts work on the board, spawns
workers, verifies what comes back, and commits. Workers get a much shorter
prompt (below); they don't need this document.

Read `docs/self-organization.md` if you want the model behind it. This is the
operational side.

## Before you start

```bash
tab-atelier tasks            # what's already on the board
tab-atelier fleet            # who is already working — don't duplicate them
free -g                      # each claude is ~250-400 MB, plus cargo builds
tab-atelier peers | wc -l    # how loaded the machine already is
```

**Claude quota is finite.** Every worker is a real session burning real tokens.
Decide the budget *before* spawning: N workers × M tasks each is the whole
cost, and workers must be told that cap in their prompt. Do not respawn a
worker that stopped — it stopped because it hit its cap.

Four workers is a reasonable default on a 12-core box with ~10 GB free. They
contend on the cargo target lock, so more than ~6 mostly wait.

## 1. Put work on the board

Work comes from **sources**: any command printing `id<TAB>title` per line.
`backlog` owns idempotence and cooling, so a source can emit its whole world
every run and re-running announces nothing new.

```bash
# module-by-module audit (ships with the repo)
tab-atelier backlog --from "$PWD/scripts/tasks-audit-modules.sh"

# coverage, from a report you already generated
cargo llvm-cov --no-default-features --features headless \
    --lcov --output-path target/lcov.info
tab-atelier backlog --lcov target/lcov.info --root "$PWD" --limit 6 --audit-largest 2

# anything else — a source is 3 lines of shell
tab-atelier backlog --from 'rg -l "TODO" src | sed "s|.*|todo:&\tclear the TODOs in &|"'
```

Task ids must be **derived from the thing worked on** (`cov:src/api.rs`), never
from a counter or a timestamp: that is what makes re-running safe and what lets
two hosts agree they are talking about the same job.

## 2. Spawn workers

```bash
P='<the worker prompt below>'
for n in 1 2 3 4; do
  tab-atelier dispatch --new --name "fleet-$n" --cwd "$PWD" "$P"
  sleep 4
done
```

`--cwd` matters: it sets what the worker sees, and it selects which per-project
brief it receives (see `docs/agent-brief.md`).

### The worker prompt

Copy this verbatim. It is one paragraph on purpose — `dispatch` types it into
the tab, and a newline would submit early.

```text
You are a worker in a self-organising fleet, in the <REPO> repo. Loop: run
`tab-atelier take` to lease a task (exit code 3 means nothing is free — then
stop and say so). If the id starts with cov: add real unit tests to that file
(its #[cfg(test)] mod tests) covering behaviour and edge cases, no trivial
assertions, each test commented with WHY it matters; then run `cargo fmt`,
`cargo test --no-default-features --features headless --lib`, and `cargo clippy
--all-targets --no-default-features --features headless` until all clean. If
the id starts with audit: READ that module and report concrete findings as
file:line + what is wrong + why it matters; do NOT change code for an audit.
Then close it: `tab-atelier done <task-id> "<one line result>"` (add --fail if
you could not do it). Take at most 2 tasks then stop. Rules: never touch a file
you did not lease; do not git commit, do not push, do not run cargo llvm-cov
(too slow); if tests fail because of someone else's edit, re-run them.
```

Why each clause earns its place:

- **`take`, not "work on X"** — the lease is what stops two workers colliding.
  Assigning work by hand throws away the whole mechanism.
- **"at most 2 tasks then stop"** — the quota cap. Without it a worker loops
  until the board is empty.
- **"never touch a file you did not lease"** — concurrent edits to one file
  produce a mess no one can review.
- **"do not git commit"** — you are the one who reviews and commits. A worker
  committing its own unverified work is how bad tests land.
- **"do not run cargo llvm-cov"** — it is a 10+ minute instrumented rebuild;
  four workers running it in parallel will wedge the machine.
- **`--fail` exists** — a worker that gives up must say so, or the task looks
  finished and nobody revisits it.

## 3. Watch

```bash
tab-atelier fleet                     # who holds what, with lease time left
tab-atelier fleet --json              # nodes + edges, for a graph
tab-atelier tasks --all | grep done   # what has closed
tab-atelier peers | grep fleet        # idle = finished its cap
```

`NO LEASE` on a `works_on` edge means the worker was awarded the task but no
longer holds the lease — it died, or thought past its expiry. Look at that tab.

To block until something finishes, use exit codes rather than watching a screen:

```bash
tab-atelier wait cov:src/api.rs --timeout 600   # 0 done, 1 failed, 3 running, 4 unknown
for t in $(tab-atelier tasks --all | grep -o '^\[[^]]*\]' | tr -d '[]'); do
    tab-atelier wait "$t" --timeout 900 &
done; wait
```

Run long waits in the background rather than blocking your own turn.

## 4. Verify — do not skip this

Workers report their own success. That is the single largest failure mode in
multi-agent systems (MAST puts task verification at 21% of failures), and it is
the job the fleet cannot do for you yet.

```bash
git diff --stat                      # what actually changed
cargo test --no-default-features --features headless --lib   # ALL of it, not just the new tests
cargo clippy --all-targets --no-default-features --features headless
cargo clippy --all-targets           # the other config too
cargo fmt --check
```

Then read the diff. Specifically:

- **A `done` does not mean the tests pass.** In the first run of this playbook,
  a worker closed its task with one test still failing. The suite caught it;
  its own report did not.
- **Tests that assert nothing** — `assert!(result.is_ok())` over a function
  that cannot fail is coverage theatre. Reject it.
- **Audit findings are claims, not facts.** Check each one against the code
  before acting. In the first run, three of three spot-checked findings were
  real, including a security-relevant doc that promised cert pinning the code
  did not do — but "three of three" is a reason to keep checking, not to stop.

Fix what needs fixing yourself, then commit once, crediting the fleet in the
message.

## 5. When you are done

```bash
tab-atelier note --topic fleet --from driver "run finished: N tasks, M findings, coverage X% -> Y%"
```

Leave the workers' tabs open if you might want their reasoning; close them if
the machine is tight. A worker that stopped holds no lease, so nothing is
blocked either way.

## Things that will bite you

- **Announcing while workers run is fine** — the board is append-only and
  `take` re-reads it. Adding work mid-run is normal.
- **A lease outlives a dead tab** (up to its TTL, default 15 min via `take`).
  That is deliberate: the work returns to the board on its own. Do not
  hand-release someone else's lease.
- **Absolute paths in task ids** break cross-machine work. Use `--root` with
  the coverage source; a source you write should emit repo-relative ids.
- **Audits do not raise coverage.** They are separate goals on one board; if
  you want coverage to move, announce coverage tasks and check that workers
  took them rather than the audits they happened to rank first.
