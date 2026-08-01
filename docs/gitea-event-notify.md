# Notifying an agent on Gitea events (no polling)

Status: **design / not yet built.** Captures the approach discussed so we don't
lose it. Goal: when something happens in Gitea (push, PR opened/updated, issue,
comment, review), the responsible agent tab should **take a turn on the event** —
without the agent sitting in a poll loop or a blocking `wait`.

## Why not just use a Gitea MCP server?

Every Gitea MCP server surveyed (the official `gitea/gitea-mcp`, plus
`amonstack/gitea_mcp`, `MushroomFleet/gitea-mcp`, `nikitatsym/gitea-mcp`) is a
**pull / request-response tool server**: it exposes Gitea operations (list/create
issues, PRs, comments, even manage webhooks) as tools the agent **calls when it
wants**. None push events to the agent.

- The most "event-driven" one (`nikitatsym/gitea-mcp`) fakes it with a
  `wait_for_*` tool that **polls Gitea in the background** — `max_poll_failures`,
  `max_lifetime` default 2h, self-terminating. That is exactly "the agent waits
  and polls," just hidden behind a tool call.
- The official repo's webhook work only *creates/lists* webhooks; it doesn't
  receive them.

**Root cause:** MCP is client-pull. An MCP server has **no channel to wake the
agent** — the model only acts when the client asks it to. So event-driven wake
cannot come from an MCP server.

tab-atelier is the right home instead: it **owns the agent tabs** and can inject a
turn (`tab-atelier dispatch <tab> "<prompt>"` types a prompt + Enter into a tab's
agent via `POST /tabs/by-id/<id>/input`). The only missing piece is a **webhook
receiver** that turns a Gitea event into a dispatch.

## Can a local workstation even receive webhooks? Yes.

Webhooks are inbound POSTs from Gitea to a URL, so the workstation just needs to
be reachable from Gitea.

- **Gitea on the same LAN / self-hosted locally:** point the webhook at
  `http://<workstation-lan-ip>:7890/hooks/gitea` directly.
- **Gitea remote, workstation behind NAT:** reuse the **Cloudflare Tunnel** that
  already exposes the API (e.g. `https://t-atelier.williamdes.eu`). Point Gitea
  at `https://<tunnel-host>/hooks/gitea`. Two wrinkles:
  - **CF Access** fronts the tunnel. Give the `/hooks/*` path a CF Access
    **Bypass** policy — the webhook authenticates itself with Gitea's **HMAC
    secret** (`X-Gitea-Signature`, `sha256=…`), verified by tab-atelier, so it
    doesn't also need to satisfy Access. (Gitea webhooks can't easily carry CF
    service-token headers; bypass + HMAC is the clean combo.)
  - Nothing polls: Gitea pushes → tunnel carries it in → tab-atelier verifies +
    dispatches.
- **No public ingress at all** (fallback, not needed here): an outbound relay —
  the workstation dials out to a small always-on relay, Gitea → relay → relay
  pushes down the held-open connection. Skip it; the tunnel already solves this.

## Proposed shape

```
Gitea event ─push→ CF Tunnel  (/hooks/gitea, Access-bypassed)
              → tab-atelier: verify HMAC → map repo→tab → dispatch "<prompt>"
                                            → agent takes a turn. No poll, no wait.
```

### Receiver: native `POST /hooks/gitea` in the daemon

- Verify `X-Gitea-Signature` (HMAC-SHA256 of the raw body with a configured
  secret) before parsing. Reject with 401 on mismatch. Constant-time compare.
- Parse the event (`X-Gitea-Event` header names it: `push`, `pull_request`,
  `issues`, `issue_comment`, `pull_request_review`, …).
- Route to a target tab (see Routing), format a concise prompt (see Prompt), and
  inject it through the **existing** input path (`pending_input`, same mechanism
  `dispatch`/`/input` use). Optionally also drop the raw JSON into the tab's
  `inbox/` (the `handoff` mechanism) so the agent can read full details.
- Respond fast (200) and do the injection async — webhooks time out.

MVP alternative (zero daemon code, to validate wording/routing first): a ~20-line
HMAC-verifying HTTP listener that shells out to `tab-atelier dispatch`. Promote to
the native endpoint once it earns its keep.

### Delivery (all primitives already exist)

- **`dispatch` / `/input`** — types the prompt in so the agent acts *now*. Claude
  Code buffers input typed while it is mid-turn, so a dedicated queue is usually
  unnecessary. This is the core "wake and act".
- **`handoff`** — drop the raw event JSON into the tab's `inbox/gitea-<id>.json`
  as a companion to the one-line prompt.
- **`note`** (blackboard) — lightweight broadcast the agent reads when it next
  looks; more pull-ish, secondary.
- **Wait-for-idle** — optional: queue the event and deliver only when the target
  agent's state LED shows idle, to avoid interrupting a live turn. Start without
  it (rely on Claude's input buffering); add only if interrupts annoy.

### Routing (repo/event → tab) — to decide

- **One triage agent** for all repos (simplest): every event dispatches to a
  single configured tab that decides what to do.
- **Per-repo tabs:** a config map `repo (owner/name) → tab name-or-id`, or a
  naming convention (tab named after the repo). Unmapped repos → the triage tab
  or ignored.
- Store the mapping in `preferences.json` (e.g. a `gitea_hooks` block: secret +
  `{repo: tab}` map + which event types to act on), set via a `settings`-style
  CLI so a headless daemon needs no file editing.

### Prompt formatting (event → instruction)

Turn the event JSON into a short, actionable instruction, e.g.:

- `pull_request` opened → `PR #12 "<title>" opened by <user> on <repo>. Review it.`
- `issue_comment` → `New comment on #7 by <user>: "<snippet>". Respond if needed.`
- `push` → `Push to <repo>@<branch> by <user> (<n> commits). Check CI / rebuild.`

Keep it one line; the full payload rides in `inbox/` when needed.

## Open decisions (blockers before building)

1. Is Gitea **local/same-LAN** or **remote via the tunnel**? (Decides whether we
   document the CF Access bypass path.)
2. **One triage agent** vs **per-repo tabs**? (Decides the routing config shape.)
3. Receiver: **external-glue MVP first** vs **native `/hooks/gitea` now**?
4. Which event types are in scope initially (PR + comment likely; push/CI later)?

## Security notes

- HMAC secret is the load-bearing auth; treat it like a token (store under
  `~/.config/tab-atelier/`, not world-readable).
- The `/hooks/*` route must **not** accept the master API token as an alternative
  (it is a distinct, secret-authenticated surface) and must **not** be able to
  reach anything but "inject a prompt into a mapped tab" — never arbitrary tabs
  or arbitrary commands.
- CF Access Bypass on `/hooks/*` only; the rest of the API keeps its Access
  policy. Rate-limit / size-cap the endpoint (webhook floods).
