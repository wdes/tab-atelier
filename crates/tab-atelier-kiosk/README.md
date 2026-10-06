# tab-atelier-kiosk

The tab-atelier web Kiosk: the decision, report and intent panes, over HTTP.

It sits in front of the daemon as its own origin. It serves the interface it embeds
(`/`, `/kiosk`, `/assets/*`) and **reverse-proxies everything else** to the daemon — so the
browser only ever sees one origin, and there is no CORS, no second token, and no second
place where the panes' routes are declared.

There is deliberately no local health path: the daemon is the only thing whose liveness
matters, and a route this server answered itself would report the Kiosk as healthy while
the thing behind it is down.

## Configuration

| Flag | Environment | Default |
| --- | --- | --- |
| `--listen` | `TAB_ATELIER_KIOSK_ADDR` | `127.0.0.1:8282` |
| — | `TAB_ATELIER_UPSTREAM` | `http://127.0.0.1:7890` (the daemon) |
| — | `TAB_ATELIER_INTENTIONS_DIR` | `$HOME/Dev/intentions` |
| — | `TAB_ATELIER_INTENT_CMD` | none — without it, intentions are created but no worker starts |

The two intention variables must be **exported**, not passed as a one-off
`env VAR=x …` prefix: the second one is optional and an unset value is a normal
state, so a variable that fails to arrive is indistinguishable from one that was
never set. `~/Dev/Botmox/kiosk-serve.sh` sets both and is the way this Kiosk is
started here.

Loopback by default: this server fronts a daemon that authenticates by token, and the
tunnel that may sit in front of it is the deployment's business, not this repository's.

## Using it

`docs/kiosk.md` in the repository root — the three tabs, the `decision` verb and
the mistakes it invites, and what each failure message means. Read it before
asking why a pane is empty.

## Why it is a separate package

Because it is a separately replaceable thing. It has its own service account, its own
systemd unit, and its own failure domain: a Kiosk that crashes should not take the daemon
down with it, and a Kiosk being upgraded should not stop tabs from working.
