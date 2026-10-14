# tab-atelier-kiosk

The tab-atelier web Kiosk: a small server that sits in front of the daemon.

It serves the interface it embeds, and relays the paths it does not own back to
the daemon — the rest of the harness routes (`/reports`, `/intent`) and the
daemon's own API (`/api/*`) alike. Same origin is the point: the page and the
calls it makes then share one origin, so the browser never makes a cross-origin
request and CORS never comes up.

It owns one pane itself. The decision journal is the Kiosk's own data, so the
Kiosk reads and writes it directly, behind its own token:

| Route | What it does |
|---|---|
| `GET /decisions[?includeArchived=true]` | the board, newest first |
| `GET /decisions/file?path=…` | one linked file, confined to the outbox |
| `POST /decisions/<id>/read` | mark a decision read |
| `POST /decisions/<id>/tranch` | rule a decision, with a verdict |

Everything else is the daemon's, relayed unchanged.

## Running it

```
tab-atelier-kiosk serve --listen 127.0.0.1:8282 --upstream http://127.0.0.1:7890
```

| Env | Default | Meaning |
|---|---|---|
| `TAB_ATELIER_KIOSK_ADDR` | `127.0.0.1:8282` | where it listens |
| `TAB_ATELIER_UPSTREAM` | `http://127.0.0.1:7890` | the daemon it relays to |
| `TAB_ATELIER_KIOSK_STATE` | `/var/lib/tab-atelier-kiosk` | state directory |
| `TAB_ATELIER_DECISIONS` | `<state>/decisions.jsonl` | the journal |
| `TAB_ATELIER_OUTBOX` | `<state>/outbox` | the tree `/decisions/file` serves from |
| `TAB_ATELIER_KIOSK_TOKEN` | *(none)* | the token the panes require |

**The token fails closed.** With no token configured the panes answer 401 rather
than serving an unauthenticated board; a kiosk meant to be looked at gets one.

## The journal

JSON Lines: one decision per line, so the file stays legible and a hand edit is a
hand edit rather than a corruption. Only `id` is required; the interface
feature-detects the rest.

```json
{"id":"a1","project":"app","state":"open","title":"Retire the v1 wire?",
 "summary":"…","whyGated":"breaks a client we still ship","files":["outbox/a1.md"]}
```

`state` is one of `open`, `read`, `tranched`, `archived`; an omitted `state` is
`open`. `tranch` sets `state` to `tranched` and records the `verdict`.
