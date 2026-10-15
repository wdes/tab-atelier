# tab-atelier-kiosk

The tab-atelier web Kiosk: a small server that sits in front of the daemon.

It serves the interface it embeds, and relays the paths it does not own back to
the daemon — the daemon's own API (`/api/*`) and anything the page learns to ask
for. Same origin is the point: the page and the calls it makes then share one
origin, so the browser never makes a cross-origin request and CORS never comes
up.

It owns the panes itself, because they are the operator's view and not the
daemon's data — each read and written directly, behind its own token:

| Route | What it does |
|---|---|
| `GET /decisions[?includeArchived=true]` | the board, newest first |
| `GET /decisions/file?path=…` | one linked file, confined to the outbox |
| `POST /decisions/<id>/read` | mark a decision read |
| `POST /decisions/<id>/tranch` | rule a decision, with a verdict |
| `GET /reports` | the outbox listing, newest first |
| `POST /intent` | record an intention (`{"content": …}`) |
| `GET /intent/list` | the intentions, the ready ones last |
| `POST /intent/new` | start an intention (`{"title", "repo", "pitch"}`) |
| `GET /intent/<slug>` | one intention, whole (front matter and turns) |
| `POST /intent/<slug>/ask` | append a turn to the intention |
| `POST /intent/<slug>/promote` | move it into `READY-intentions/` |
| `POST /kiosk/remotes/<id>/intent/<slug>/launch` | open a worker tab on a remote for an intention |
| `GET /catalog/list` | the skills catalogue (`{retired, skills}`) |
| `GET /kiosk/remotes` | the servers the Kiosk aggregates (tokens redacted) |
| `POST /kiosk/remotes` | add a server (label, url, token) |
| `POST /kiosk/remotes/<id>/remove` | forget a server |
| `GET /kiosk/remotes/<id>/tabs` | a remote's tabs, reached with its token |
| `POST /kiosk/remotes/<id>/tabs` | open a tab on a remote |

Everything else is the daemon's, relayed unchanged. A path in the table reached
with the wrong method is a **405** with an `Allow` header — not a relay to the
daemon, whose auth gate would answer a misleading 401 for what is really "this
method is not allowed here".

## Several servers

The Kiosk's point is to take advantage of **several resources at once**, so it
holds a list of remotes — each a daemon it can reach, with the token that opens
it. The list lives on the Kiosk (`<state>/remotes.json`, `0600`), set from the
**serveurs** screen (`/kiosk/manage`): add a label, an API URL and a token, and
the Kiosk reaches that server **server-side**. The browser holds only the Kiosk
token; a remote's token is never sent to a page, so a cross-site script on the
Kiosk origin cannot walk off with the fleet's credentials. This mirrors the
desktop app's `tab-atelier remote add`.

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
| `TAB_ATELIER_REMOTES` | `<state>/remotes.json` | the servers the Kiosk aggregates |
| `TAB_ATELIER_INTENTIONS` | `<state>/intentions` | the intentions directory |
| `TAB_ATELIER_KIOSK_TOKEN` | *(generated)* | the token the panes require |

**The token is provisioned on first start.** With no `TAB_ATELIER_KIOSK_TOKEN`,
the kiosk generates one into `<state>/token` (`0600`, reused on restart) and logs
a copy-pasteable `http://…/?token=…` at startup. Set the env var to override it
(a file is not written). The panes **fail closed**: if a token cannot be resolved
at all — an unwritable state directory — they answer 401 rather than serving an
unauthenticated board.

```bash
sudo cat /var/lib/tab-atelier-kiosk/token          # read the board token
# then open:  http://127.0.0.1:8282/?token=<that>
```

### The outbox

Two things live under the state directory. The journal above, and the outbox —
the markdown the reports tab lists and the intentions the intent tab writes:

```
<state>/decisions.jsonl     the board
<state>/token               the pane token (0600)
<state>/remotes.json        the servers, with their tokens (0600)
<state>/intentions/*.md     intentions, and READY-intentions/ once promoted
<state>/outbox/*.md         reports
```

### Backup and restore

The package never deletes the state directory — not even on purge — but a move to
new hardware, a reinstall, or a mistake at the terminal still loses data unless it
can be taken out and put back. That is what these do; the destination is **yours
to choose**, never defaulted to `/var`:

```bash
tab-atelier-kiosk backup ~/kiosk-backup            # copy the whole state dir
tab-atelier-kiosk restore ~/kiosk-backup           # put it back (refuses a non-empty state)
tab-atelier-kiosk restore ~/kiosk-backup --force   # ... over live data
```

`--state <dir>` overrides the source and target; `TAB_ATELIER_KIOSK_STATE` sets
the default.

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
