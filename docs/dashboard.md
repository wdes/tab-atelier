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

### `context` grammar

A tab declares its place with `set-context "<phase>/<role>/<item>"`:

- `phase` : one of the canonical node ids above. Required to map to a node.
- `role`  : free label of the agent role (e.g. `implementer`, `reviewer`,
  `verifier`, `scoper`). Shown in the hover popup.
- `item`  : free short label of the current unit of work, the "five words"
  (e.g. `slice-2-rust-state`). Shown in the hover popup.

A tab whose `context` does not start with a known phase id is listed under an
`unmapped` bucket, never silently dropped.

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
`/tabs/usage`, parses each tab's `context` into a phase, groups tabs by node,
and computes `rollupLed`. Shape:

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
          "context": "build/implementer/slice-2-rust-state",
          "role": "implementer",
          "item": "slice-2-rust-state",
          "agentState": "thinking",
          "led": "working",
          "tokens": { "input": 12345, "output": 6789 },
          "viewerUrl": "/tabs/by-id/<uuid>/view"
        }
      ]
    }
  ],
  "unmapped": [ /* same tab shape, for tabs with no/unknown phase */ ]
}
```

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
