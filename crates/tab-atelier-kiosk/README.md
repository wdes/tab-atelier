# tab-atelier-kiosk

A web Kiosk: one view over **several tab-atelier instances at once**.

It is not a front for one daemon. Each project is independent and its own
install, and the Kiosk reaches each one over its HTTP API — with that instance's
own URL and token. There is no central daemon and no `--upstream`: a machine's
tabs show up because it was added as a remote, which is the same configuration
that reaches it. The crate never imports another crate's types; the HTTP API is
the whole contract, which is what lets the pieces talk without being welded
together.

It serves the interface it embeds and owns the panes whose data is the
operator's — each read and written directly, behind its own token:

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
| `GET /kiosk/remotes` | the servers the Kiosk shows (tokens redacted) |
| `POST /kiosk/remotes` | add a server (label, url, token) |
| `POST /kiosk/remotes/<id>/remove` | forget a server |
| `GET /kiosk/remotes/<id>/tabs` | a remote's tabs, reached with its token |
| `POST /kiosk/remotes/<id>/tabs` | open a tab on a remote |
| `GET /kiosk/remotes/<id>/cards` | a remote's agent cards, from its tabs' meta |
| `GET /kiosk/fleet` | every server and its tabs, aggregated server-side |

**The entry point is the landing page**, which is the fleet: every configured
server and its tabs, each with an "Ouvrir un onglet" form. To see a machine's
tabs, add it as a remote — that is the only way and the only place, by design.
Anything the table does not list is a **404**, not a relay: there is no single
daemon to fall back to. A path in the table reached with the wrong method is a
**405** with an `Allow` header.

## Several servers

The Kiosk's point is to take advantage of **several resources at once**, so it
holds a list of remotes — each a tab-atelier it can reach, with that instance's
token. The list lives on the Kiosk (`<state>/remotes.json`, `0600`), set from the
**serveurs** screen (`/kiosk/manage`): add a label, an API URL and a token, and
the Kiosk reaches that server **server-side**. The browser holds only the Kiosk
token; a remote's token is never sent to a page, so a cross-site script on the
Kiosk origin cannot walk off with the fleet's credentials. This mirrors the
desktop app's `tab-atelier remote add`.

## Running it

```
tab-atelier-kiosk serve --listen 0.0.0.0:8282
```

| Env | Default | Meaning |
|---|---|---|
| `TAB_ATELIER_KIOSK_ADDR` | `0.0.0.0:8282` | where it listens |
| `TAB_ATELIER_KIOSK_STATE` | `/var/lib/tab-atelier-kiosk` | state directory |
| `TAB_ATELIER_DECISIONS` | `<state>/decisions.jsonl` | the journal |
| `TAB_ATELIER_OUTBOX` | `<state>/outbox` | the tree `/decisions/file` serves from |
| `TAB_ATELIER_REMOTES` | `<state>/remotes.json` | the servers the Kiosk shows |
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

**What this covers, and what it does not:** the Kiosk's own state — the board,
the intentions, the reports, the remotes and their tokens. A tab's **meta**, and
so its agent card, is the *daemon's* data, kept in the daemon's own state
directory and restored by the daemon on startup; this tool neither reads nor
writes it. Back that directory up separately if you want it in the same place.

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

## Developing it

The Kiosk is a standalone server, so you test your own edit without packaging it:

```bash
crates/tab-atelier-kiosk/scripts/hotswap.sh            # build, test, install, restart
crates/tab-atelier-kiosk/scripts/hotswap.sh --no-test  # skip the tests, for a quick look
```

It builds the release binary, runs the kiosk tests, installs it over
`/usr/bin/tab-atelier-kiosk`, and restarts the unit. It uses sudo (it writes
`/usr/bin` and restarts the system service), so it asks for your password. A
change is live in seconds; a `.deb` is only for shipping the Kiosk to someone
else (`apt install` overwrites the hotswapped binary — just run the script again
after pulling).

The crate is laid out by what changes for its own reason:

| Module | What it owns |
|---|---|
| `assets.rs` | the interface, compiled in with `include_str!` |
| `config.rs` | the bind address, the state directory, the pane token |
| `route.rs` | where a request goes — its assets, its panes, or a 404 |
| `panes.rs` | the pane handlers and the token that opens them |
| `decisions.rs` · `reports.rs` · `intent.rs` · `card.rs` | the data the panes read and write |
| `remotes.rs` · `daemon.rs` | the servers it reaches, and the client that speaks their API |
| `server.rs` | the socket loop |
| `backup.rs` | copy the state directory out, and back |
| `assets/kiosk.{html,css,js}` · `assets/manage.*` | the interface itself |

The daemon it talks to is untouched by a kiosk change; the two share only the
HTTP API.
