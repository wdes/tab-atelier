# Dashboard (harness control panel)

Real-time control panel for the multi-agent harness: "which agent works on
what, on which project step". A workflow diagram, served by the daemon, whose
nodes light up live from the state of the tabs. GUI for humans, JSON for agents.

Anchor issue: https://github.com/wdes/tab-atelier/issues/38

The JSON half already exists (`GET /tabs/usage`). This feature adds a mapped,
aggregated view (`GET /dashboard/state`) and a static web app (`GET /dashboard`)
that renders it.

## Shared language

Fixed vocabulary so tabs, the Rust mapper, and the web app never diverge.

### Phase nodes (the canonical skeleton)

The diagram skeleton is the project-phase flow (autoprompt Plan / Build /
Final-checks). Seven canonical node ids, in order:

| id       | phase                        |
|----------|------------------------------|
| `scope`  | Scope + executable roadmap   |
| `plan`   | Detailed plan (when needed)  |
| `build`  | Implement (test first)       |
| `review` | Implementation review        |
| `verify` | Tests + verification         |
| `sweep`  | Sweep + goal check + sign-off|
| `done`   | Done                         |

A node is drawn once and shows the tabs currently occupying it as its
occupants (the L0-L4 "who works on what" dimension). Per-run adaptations
(extra or renamed nodes) come from an optional run manifest, v2, not v1.

### Three sources of truth per tab

A tab carries three dimensions **from distinct sources**, so none clobbers
another (Increment 2):

- **`assignment`** (persistent, hook-immune) — the agent's place in the
  workflow: `assignment = "[<project>:]<phase>/<role>"`. Set **once** via
  `set-assignment`, never touched by the prompt hook. `<phase>` ∈ {scope, plan,
  build, review, verify, sweep, done}. `<role>` is a free label (`implementer`,
  `reviewer`, `planner`, `orchestrator`, `auditor`…). The optional `<project>:`
  prefix **overrides** the project derived from cwd.
- **`cwd`** — the **project**: basename of the repo (`~/Dev/kalpin-back` →
  `kalpin-back`). A dev work-root (`~/Dev`, `/src`…) or no cwd → the **`divers`**
  lane; an itinerant meta-specialist → the **`méta`** lane (below).
- **`context`** — the **"five words" subtitle**: the current prompt, rewritten
  by the `user-prompt` hook every turn. **Volatile by design** — "what this tab
  is doing right now".

The dashboard maps a tab onto a **phase node via `assignment`** (never via the
volatile `context`), buckets it under a **project via `cwd`/override**, and
labels it with **`context`**. A tab with **no `assignment`** stays `unmapped`
(never dropped) but still appears under its project at level 0.

### Project dimension + `méta` lane

Project resolution, in order: (1) the `<project>:` override; (2) basename of a
repo cwd; (3) the **`méta`** lane if the role is meta-class (planner, auditor,
tichef, orchestrator with no repo); (4) **`divers`**. Projects are sorted
alphabetically with `méta` then `divers` pinned last. Each project carries
`tabCount`, `rollupLed` (worst led of its tabs), `hasOrchestrator`, `isMeta`,
and its own scoped 7-node subtree (`nodes`/`unmapped`).

`ponytail:` project = basename of cwd, no git-root detection — a tab in a
subfolder maps the subfolder; upgrade = walk to the enclosing `.git`.

### `led` rollup

Each tab already carries a synthesized `led`: `dead | error | working |
unreviewed | idle`. A node's `rollupLed` is the worst-severity led among the
tabs mapped to it, by this precedence (worst first):

```
dead > error > working > unreviewed > idle
```

An empty node (no tabs) has `rollupLed: null` and renders neutral.

## `GET /dashboard/state`

Rust endpoint (unit-tested, counts toward codecov). Reads the same source as
`/tabs/usage`, maps each tab onto a phase node **via `assignment`**, buckets it
under a **project**, and computes `rollupLed`. Shape:

```json
{
  "nodes": [
    {
      "id": "build",
      "rollupLed": "working",
      "tabs": [
        {
          "id": "<uuid>",
          "name": "ta-rust-builder",
          "assignment": "build/implementer",
          "role": "implementer",
          "context": "wiring the parser",
          "item": "wiring the parser",
          "agentState": "thinking",
          "led": "working",
          "tokens": { "input": 12345, "output": 6789 },
          "viewerUrl": "/tabs/by-id/<uuid>/view"
        }
      ]
    }
  ],
  "unmapped": [ /* same tab shape, for tabs with no/unknown assignment */ ],
  "projects": [
    {
      "name": "kalpin-back",
      "tabCount": 2,
      "rollupLed": "working",
      "hasOrchestrator": false,
      "isMeta": false,
      "nodes": [ /* the 7-node subtree, scoped to this project */ ],
      "unmapped": [ /* this project's unassigned tabs */ ]
    }
    /* … sorted alpha, "méta" then "divers" pinned last */
  ]
}
```

`role` and the phase come from `assignment` (never the volatile `context`);
`item` is now the `context` subtitle. The global `nodes`/`unmapped` are the
Increment 1 contract, preserved.

### Lineage & altitude (S6)

Each tab also carries `parentTabId` (the UUID of the tab that spawned it —
`dispatch --new` stamps it via `POST /tabs/by-id/{id}/parent`, reading `_TAB_ID`)
and a static `altitude` band from its role class (`0` tichef, `1` orchestrator,
`2` worker/specialist). `DashboardState.lineage` is the deduped list of
`{child, parent}` delegation edges whose parent is a known tab — an unknown or
self parent degrades to a root (no edge), so no cycle survives. A root tab has
no `parentTabId` and falls back to its role altitude.

### Re-home status (predecessor lifecycle)

A tab being re-homed (`~/Dev/Botmox/rehome-tab.sh`) carries a `rehomeStatus`
through its bidirectional-proof loop: `handoff-written` → `successor-ready` →
`ack-sent` → `safe-to-close`. `rehome-tab.sh` stamps the first three via
`tab-atelier set-rehome-status <state> --tab <old-uuid>` (route `POST
/tabs/by-id/{id}/rehome`, only the 4 states accepted); the old agent posts
`safe-to-close` on itself when it replies REHOME ACK — the final proof from the
predecessor's own side. It drives a progress badge on the predecessor tab and
gates the right-click "close the predecessor" action (enabled only at
`safe-to-close`; never auto-closes — the human still clicks). The successor's
`parentTabId` is set to the old uuid so the dashboard draws the old→new edge.

### Orchestrators, unassigned, activity (Increment 5)

- Each `DashboardProject` carries `orchestrators[]` — the orchestrators working
  in that repo, sorted by id, each with `id`, `name`, its current `item` (the
  volatile context) and a GLOBAL `childCount` (how many tabs it spawned, counted
  via `parentTabId` across every repo).
- `DashboardState.unassigned[]` is the top-level bucket of tabs with **no
  `assignment` at all** (sorted by id) — legitimately un-placed. It's distinct
  from `unmapped` (assigned but an unknown phase): an unknown-phase tab is
  unmapped but never unassigned.
- `GET /dashboard/activity` is a thin passthrough of the activity scribe's
  `~/.local/state/tab-atelier/activity.json` — verbatim when present, a graceful
  empty `{}` when absent or malformed (never 404/500). Same auth gate as
  `/dashboard/state` (master or the dashboard share-token; 401 otherwise).

### Serving + services (Increment 6)

- Each `DashboardTab` carries `serving` — the assignment's `<project>:` override,
  i.e. the team a méta is currently serving (so it's busy, not available). `None`
  when there's no override; skipped from the wire.
- `DashboardState.services[]` groups the flat `projects` into repo families: a
  shared prefix before the first `-` (≥2 repos), or an explicit
  `Preferences.repo_families` map (e.g. `{"louis":"kalpin"}`), forms a named
  service; a lone repo stays a "mono" service under its own name. Each carries a
  `rollupLed` (worst led of its sub-repos) and its member repo names. `projects`
  is kept alongside (non-breaking).
- The `user-prompt` hook also mirrors a **genuine human direction** onto the
  `direction` blackboard topic (never a cron/watcher tick, nudge, synthetic
  injection, or flag) so the fleet knows where the PO stands.

## `GET /dashboard`

Serves the static web app (`assets/dashboard.html` + `assets/dashboard.js` +
`assets/dashboard.css`, compiled in via `include_str!` like the viewer). Vanilla
JS, inline SVG skeleton, no build step. It polls `/dashboard/state` every ~1.5s
and:

- highlights each phase node by its `rollupLed` (CSS class per `node-id`),
- on hover, shows a popup listing each occupying tab as
  `{name, item, role, agentState, tokens}`,
- on right-click of a tab entry, opens its `viewerUrl`.

Headless tabs appear exactly like GUI tabs (they are in `/tabs/usage` too).

## Auth

The dashboard is reached with a **global, read-only share token** — modelled on
the tab viewer's share link so it can be handed to a remote browser later (like
the mobile onboarding QR). It is **global** (one token for the whole panel, not
per-tab) and **read-only** (the dashboard never sends input, so there is no
RW/RO split — the day it grows actions, that path will need a separate RW
token).

- **Gated routes**: `GET /dashboard` (the HTML page) and `GET /dashboard/state`.
  Both accept the **master token** OR the **dashboard share-token**, passed as
  `?token=…` (browser-friendly) or `Authorization: Bearer …`, compared in
  constant time. The static assets (`/assets/dashboard.{js,css}`) stay public —
  the page loads them before its JS reads the token from the URL.
- **Getting the link**: `tab-atelier-headless share-link --dashboard` prints
  `http(s)://host:port/dashboard?token=<dashboard-token>`. The token is minted
  lazily on that first request and **persisted in `tabs.json`**, so a shared
  link survives a daemon restart.
- **Revoking**: `POST /tabs/rotate-tokens` clears the dashboard token (alongside
  every per-tab share token); the shared link 401s until a new one is minted.

## Acceptance (verified in GUI before ship)

Per the harness rule: a GUI feature is done only when the intent is observed on
screen. A GUI-specialist agent verifies these, reading the render with
browser-bridge and driving hover/right-click with Playwright.

1. Two+ tabs with a `context` published: opening `/dashboard` shows the phase
   diagram with each tab highlighted on the node matching its `context` phase.
2. A tab flips `led` working -> error: the node recolors within one poll
   (~1.5s), no manual reload.
3. Hovering a node with tabs shows a popup listing `{name, item (five words),
   role, agentState, tokens}` per tab.
4. Right-clicking a tab entry opens that tab's viewer (`/tabs/by-id/{uuid}/view`).
5. Headless workers appear alongside GUI tabs, same treatment.
