<!-- SPDX-License-Identifier: MPL-2.0 -->

# tab-atelier-proxy

A Claude tab can send its Anthropic API calls somewhere else instead of
straight upstream. `tab-atelier-proxy` is that somewhere: one machine holds a
Claude login, and everyone else authenticates to it with a key of their own.

## Why it is a separate package

It used to be a role of tab-atelier itself — `relay egress` — and it
authenticated the whole fleet with **one shared token**. That single secret:

- cannot say who spent the quota;
- cannot be taken away from one laptop without re-keying every other machine;
- keeps working for a laptop that has left the building.

An account per person with a key each answers all three. And the far end is a
*server*: it belongs on a box that has no display, run by its own system user,
under a systemd unit — not something you should have to install a terminal
emulator and X11 to obtain.

**The near end did not move.** Both tab-atelier editions — desktop and headless
— still speak `relay on`, `relay off` and `relay via`. They just point at this.

## Three credentials, and none of them is the same thing

| | what it opens | where it lives |
|---|---|---|
| **user key** (`tap_…`) | the Anthropic path, and nothing else | a developer's environment |
| **admin token** (`tap_…`) | the account API and the web UI | the proxy host only |
| **Claude OAuth login** | Anthropic itself | the proxy host only, never sent to a client |

A user key travels — it ends up in shell profiles and CI secrets — so it must
not be able to administer anything, and it is not replayable against Anthropic
directly. Presenting the admin token on the proxy path is refused, and says so.

## Install

```sh
sudo apt install tab-atelier-proxy
# Log in once — this is the egress login. HOME is set explicitly on purpose:
# see below.
sudo -u tab-atelier-proxy env HOME=/var/lib/tab-atelier-proxy claude
sudo systemctl enable --now tab-atelier-proxy
```

It listens on `127.0.0.1:7900`. Put a TLS terminator in front rather than
exposing it directly — it carries keys.

### `HOME` is the whole trick

The service runs as `tab-atelier-proxy` with `Environment=HOME=/var/lib/tab-atelier-proxy`
in its unit, and reads `$HOME/.claude/.credentials.json`. A hand-run command
inherits none of that: `sudo -u tab-atelier-proxy claude` runs *as* the right
user but does not reliably reset `HOME`, so the login lands in the invoking
user's home and the proxy never sees it. The symptom is a successful `claude`
login on the host followed by `OAuth access token has been revoked` — or
nothing at all — from the proxy.

`tab-atelier-proxy ping` prints the file it actually read, which is the fastest
way to tell the two apart:

```
  credential      —         0 ms   read /var/lib/tab-atelier-proxy/.claude/.credentials.json
```

### If the host cannot run `claude`

The login above needs a browser and a TTY, which a server often has neither of.
Copy one in from a machine that does:

```sh
ssh proxy-host sudo -u tab-atelier-proxy tab-atelier-proxy import-credentials < ~/.claude/.credentials.json
ssh proxy-host sudo systemctl restart tab-atelier-proxy
```

It validates before writing, so a wrong paste is refused rather than
overwriting a working login, and the file lands `0600`.

**This copy has a lifetime.** A refresh rotates the refresh token and
invalidates every other copy of it, so logging in again on the laptop revokes
the proxy's. When that happens every call comes back:

```json
{"type":"error","error":{"type":"authentication_error","message":"OAuth access token has been revoked."}}
```

and the admin dashboard shows the plan monitor as not reporting. Re-run the
import. Two machines sharing one Claude login is the underlying constraint, not
something the proxy can paper over.

**If the file is not there at all** — never imported, or the service started with
a `HOME` that has never held a login — the provider is disabled rather than
merely broken. It is not a routing candidate, so a request is answered `503`
naming the file that would fix it, instead of being forwarded into an egress that
fails to read it; and the plan-pressure panel is not drawn, because a subscription
that cannot authenticate is not spending the plan and there is nothing to report
on. It comes back by itself the moment the file exists: nothing was written down,
so there is no setting to undo and no restart to remember. The same applies to a
provider switched off by hand — the graph goes with it, on the judgement that in
both cases the proxy is not spending that plan.

## Accounts

```sh
sudo -u tab-atelier-proxy tab-atelier-proxy add Ada Lovelace ada@example.org
sudo -u tab-atelier-proxy tab-atelier-proxy add-key ada@example.org laptop
sudo -u tab-atelier-proxy tab-atelier-proxy list
sudo -u tab-atelier-proxy tab-atelier-proxy disable ada@example.org   # suspend the person
sudo -u tab-atelier-proxy tab-atelier-proxy remove  ada@example.org   # forget them entirely
```

`disable` and `remove` differ on purpose: disabling keeps the name attached to
past usage, deleting forgets it.

## One key per place, not one per person

```sh
tab-atelier-proxy add-key    ada@example.org laptop   # prints the key once
tab-atelier-proxy add-key    ada@example.org ci
tab-atelier-proxy keys       ada@example.org
tab-atelier-proxy remove-key ada@example.org laptop   # the rest keep working
```

```
  ci               active    first 3 min ago      last from 203.0.113.7
  laptop           active    first just now       last from 198.51.100.4
```

Adding someone mints **no** key — `add-key` does, and it needs a place to name
it after. There used to be one called `default` created at signup, which was
reliably the key that got deployed everywhere, unnamed: the accounts that most
needed a key per machine were the ones that never got one.

A person holds several keys because that is what makes revocation usable: with
one key each, losing a laptop means re-keying everything that person runs;
with a key per place, it means deleting one row.

**The dates and the address belong to the KEY, not the person** — "last used
from 203.0.113.7" says nothing when three keys share an account. *Issued and
never used* is the state worth looking for: either someone never took up their
access, or the key went astray on the way to them.

There is no `rotate`. Add the new key, deploy it, then delete the old one —
rotation revoked the only credential and issued another, so there was a moment
when everything using it was broken at once.

**A key is shown once.** The proxy stores a SHA-256 of it and cannot show it to
you again — lost key, `add-key` a replacement and `remove-key` the old one.
(A fast hash is right here: the key is 32 bytes
of CSPRNG output, so there is no dictionary to run against it, and a slow KDF
would only add latency to every proxied request.)

## The proxy picks the model

Nobody using this chooses a model. A client asks for one, and that name says
what KIND of work it is — quick, ordinary, or hard — not which endpoint should
bill it. Only the proxy knows that the five-hour window is 97% spent, or that
one provider is refusing while another is answering.

So it does three things, in order:

1. **Map.** A name the operator has rewritten is honoured verbatim, when
   something usable serves it. See *Model mapping* below.
2. **Reroute.** Same class of model, different provider. Bedrock and Vertex
   serve the *same* models from *different* quota pools, so a saturated
   subscription is a reason to move the work, not to make it worse.
3. **Degrade**, only when nothing is left that can serve the class — a cheaper
   class, trading quality for getting an answer at all.

Never upward: idle capacity in an expensive class does not promote a request
that asked for something cheap. Never silently: the response carries
`x-tab-atelier-proxy-route: <provider>/<model>`, plus `-mapped`, `-rerouted` or
`-degraded` naming what was asked for — three words because they mean three
things: a mapping was somebody's decision, a reroute preserved the answer, a
degrade did not.

Providers live in `providers.json` beside the accounts, written on first run:

```json
{
  "providers": [
    {
      "id": "anthropic",
      "base_url": "https://api.anthropic.com",
      "auth": {"kind": "claude_oauth"},
      "preference": 0,
      "models": [
        {"id": "claude-haiku-4-5-20251001", "class": "fast",     "relative_cost": 100},
        {"id": "claude-sonnet-5",           "class": "balanced", "relative_cost": 300},
        {"id": "claude-opus-5",             "class": "heavy",    "relative_cost": 1500}
      ]
    }
  ]
}
```

`relative_cost` is only ever compared with other entries, so these are the
cache-miss input figures per 1M tokens — the number that actually decides a
reroute. Add a second provider with `"auth": {"kind": "api_key_env", "var":
"SOME_KEY"}` and a higher `preference`. The key is named, not inlined — a
credential in a config file is a credential in a backup. A provider whose
credential does not resolve is never offered, because routing to it would
produce a 401 from somewhere nobody was looking.

**Every provider must speak the Anthropic Messages API.** Claude Code speaks
it, so that is the contract on the way in, and providers that share it — the
same models on Bedrock or Vertex, and the third parties that ship an
Anthropic-compatible endpoint for exactly this purpose — can be swapped by
changing a URL, a credential and a model name. Nothing is translated, so tool
use, prompt caching and extended thinking pass through untouched. An
OpenAI-shaped provider would need the request and the streamed response
rewritten, and tool-call semantics do not survive that intact — which for
Claude Code, whose every turn is tool use, is a reroute that silently breaks
the client. That adapter is a separate piece of work, not another base URL.

## What Anthropic sees

The client on the far side of the proxy *is* Claude Code, and Anthropic's OAuth
path is for Claude Code. So the proxy forwards the client's own identity rather
than substituting its own: the `claude-cli/…` User-Agent, `x-app`, the session
id, the SDK's `x-stainless-*` telemetry headers and the client's
`anthropic-beta` flags all travel unchanged. It used to rebuild each request
from scratch, which replaced that fingerprint with `tab-atelier-proxy/0.5.0`
and dropped the session id — so every call looked like an unknown client, and a
support question about one session could not be traced through.

What does **not** travel is anything scoped to a different hop: the user key
(`x-api-key`/`Authorization`, which authenticates to the *proxy* and is not an
Anthropic credential), cookies, and the `CF-Access-*` pair. That list is an
allowlist rather than a denylist, because a denylist forgets.

A client that sends none of those headers — a `curl` smoke test, a different
SDK — still reaches Anthropic looking like Claude Code: the proxy fills in what
is missing instead of either overriding a real client or sending nothing.

Every endpoint, header and credential the proxy uses lives in one crate,
`crates/claude-api`, shared with the desktop package and `catbus-agent`. There
were three copies of that list once and they had drifted apart.

## Inspecting what was actually sent

When a call misbehaves the one thing nobody can see is the request. The client
builds it, the proxy reshapes it — routing rewrites the model, `anthropic-beta`
is merged, the credential is swapped — and what goes on the wire exists for a
few milliseconds inside a blocking task. **Inspect requests** in the web UI
records it.

A capture carries the request as sent, the status that came back, and the
**token counts** — input, output and cache — read off the same response parse
the billing path uses, so the panel and the usage graph cannot disagree. That
pairing is the point: "why was that turn expensive" is answered by four numbers
beside the request that produced them.

It is off, and it turns itself off. A capture is a prompt, and a prompt is
whatever someone was working on, so:

* armed explicitly, for a stated number of minutes, up to 60;
* **it disarms itself** — "remember to switch it off" is not a control, and a
  debug flag left on is how a month of everyone's prompts ends up in a file;
* 25 captures, 4 MB each, stored whole;
* `inspect.jsonl` beside the accounts, `0600`, and an armed window never
  survives a restart;
* admin-only. A user key cannot read captures, not even its own account's.

The 4 MB cap is a disk guard, not a budget: an earlier 32 KB one made the
panel nearly useless, because a body cut in half is not JSON and reading the
object that was sent is the one thing anybody opens it for. A capture lands
when the call *finishes*, so the panel has a **Refresh** — without it the
request you just made is the one thing not shown.

Credentials are removed before anything is written. Headers are an allowlist,
so `Authorization` and `x-api-key` are absent by construction rather than by a
rule someone could forget to update, and both bodies are additionally swept for
`sk-ant-…` and `tap_…` runs — because a prompt can contain a key that no header
rule would catch, and "why is my key not working, here it is" is exactly the
kind of session that gets inspected.

## A second provider

`providers.json` takes more than the subscription. Add one from the UI, or
paste this shape in by hand:

```json
{
  "providers": [
    { "id": "deepseek", "base_url": "https://api.deepseek.com/anthropic",
      "auth": {"kind": "api_key_file", "path": "/var/lib/tab-atelier-proxy/provider-deepseek.key"},
      "preference": 10, "enabled": true,
      "models": [
        {"id": "deepseek-flash", "class": "balanced", "relative_cost": 15,
         "price": {"cache_hit": 3000, "input": 150000, "output": 600000}},
        {"id": "deepseek-v4-pro", "class": "heavy", "relative_cost": 66,
         "deprecated": true,
         "note": "withdrawn 2026-09-14; requests are served by deepseek-flash at Flash prices"}
      ],
      "peak": {"multiplier_percent": 200,
               "windows": [{"weekdays": [1,2,3,4,5], "start_hour": 1, "end_hour": 4},
                           {"weekdays": [1,2,3,4,5], "start_hour": 6, "end_hour": 10}],
               "holidays": [{"name": "Mid-Autumn Festival",
                             "dates": ["2026-09-25", "2026-09-26", "2026-09-27"]}]} }
  ]
}
```

**Every provider must speak the Anthropic Messages API.** DeepSeek's endpoint
for this is `/anthropic`, not its bare host — the bare host is the OpenAI-shaped
API, and pointing at it mangles every tool call rather than failing. Nothing is
translated on the way through, so tool use, prompt caching and thinking arrive
intact. A true OpenAI-wire adapter is still separate work, and the reason is in
`provider.rs`: tool-call semantics do not survive the round trip.

`api_key_file` rather than a key inline, because `providers.json` is the file an
operator copies around and pastes into a bug report. The key gets its own
`0600` file, written by the UI, read per request — so rotating it is a file
write, not a restart. `api_key_env` still works for a key already in the unit's
environment.

**`deprecated` is not decoration.** `deepseek-v4-pro` is withdrawn on
2026-09-14, after which requests to it are served by a *different model* at a
*different price*. A router that kept offering it would report a cost and a
capability that are both about to stop being true, so deprecated models are
listed, never routed to, and never probed.

### What an hour cost

`price` is the model's published rate per 1M tokens — cached input, uncached
input, generated output. When a request is served, the amount is **computed once
from that triple and stored beside the tokens it paid for**, in the hour bucket
it belongs to, with the peak multiplier of the hour it was served in. The
dashboard then draws the stored figure. It never prices anything at draw time,
because a rate read from *today's* table and applied to a *month-old* token
count is a number nobody was ever billed — and for a vendor that moved a price
in between, it is wrong by exactly the amount that moved.

**A model with no `price` records no money.** Not zero: nothing. `$0.00` is a
claim that the tokens were free, and absence is the truth — the hop counts its
tokens and its hours draw a gap. The subscription hop does this because a flat
plan has no per-token cost to state, and the metered models the presets list
without a rate do it too. `relative_cost` cannot stand in: it is one scalar the
router orders providers by, and a real triple is needed to bill — `deepseek-v4-pro`'s
shape across hit:miss:out is 1:30:90 against Flash's 1:50:200, so scaling one
model's rates by another's ratio is wrong on two of the three.

The rates in `providers.json` are the copy that bills, so they have to survive
a save. The provider form rebuilds every model from `id:class:relative_cost`
text, which has no room for a triple, and a provider written before the field
existed deserialises without one — either way the row keeps serving and stops
pricing. A save carries an existing rate over by model id, and a row that has
none takes back the rate the shipped catalogue publishes for that model id, as
it is loaded. That lookup can only return rates this repository actually records,
so a deliberately unpriced model stays unpriced and a rate set by hand is left
alone. A provider that ends up with no rate at all is named in the log at load:
nothing errors and no token is lost, which is exactly why the one symptom — a
money figure that never appears — has to be said out loud.

### Peak pricing

A provider can charge more for the same tokens at certain hours — DeepSeek
doubles from 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday. That is a real
difference in a comparison whose whole job is ordering providers by cost, so it
is modelled rather than written in a comment: `relative_cost` stays one true
number and the schedule explains itself. `ping` and the UI both say when a
provider is in peak right now.

**The weekday test is not the whole rule.** DeepSeek's footnote reads "excluding
Chinese public holidays", and calls a holiday off-peak *in full* — the whole
date, both windows, not the peak hours inside it. `holidays` is that exclusion:
a list of named civil days, matched against the date in the provider's own
calendar (UTC+8, fixed, because the mainland has kept one offset since 1991).
Without it every Chinese public holiday that falls on a weekday is charged
double — roughly nineteen days a year. The adjusted working weekends around a
holiday (the "make-up days") need no entry of their own and have none: every one
of them falls on a Saturday or Sunday, so it is already outside the
Monday-to-Friday windows. Weekends and holidays alike are off-peak, make-up
weekends included.

A gazette declares one year. Past the last declared holiday the calendar is
silently out of date and nothing errors — the price is merely too high on the
handful of weekdays a year that are holidays, which is the kind of thing nobody
notices. So a peak schedule with no holiday in the current year is named in the
log at load, the same way an unpriced provider is.

Like a rate, a calendar survives a save only because it is put back: the
provider form has no field for one, so a save writes the row without it, and the
row takes the catalogue's calendar back as it loads. Only an empty list is
filled, so a calendar set by hand is left alone.

### Pinning someone to a provider

The **Routed to** column sets an account's provider. An empty value means
normal routing; a value is enforced, not preferred:

```sh
sudo -u tab-atelier-proxy tab-atelier-proxy set-provider ada@example.org deepseek
```

An account pinned to a provider that is disabled or out of capacity gets a
**503**, not a quiet fall back to the subscription the operator was keeping
them off. That is the point of a pin: it is a statement about where someone's
work is allowed to go — a jurisdiction, an invoice, a quota.

### Model mapping

One global table, applied before routing. "When someone asks for the name on
the left, use the one on the right."

```
claude-opus-5  →  deepseek-flash      across providers
claude-opus-5  →  claude-sonnet-5     within one — a deliberate downgrade
```

A mapping names a **model**, not a lock. When the destination is a model only
one provider serves, the request goes there whatever the preference order says;
when that provider is unreachable, normal routing applies instead. That is what
stops a cost-control measure from turning into an outage the first time the far
end rate-limits.

## Is it the proxy, or is it Anthropic?

```sh
tab-atelier-proxy ping --count 3
```

```
upstream: https://api.anthropic.com
  credential      —         0 ms   read /var/lib/tab-atelier-proxy/.claude/.credentials.json
  connect       200       288 ms   DNS + TCP + TLS to the API host
  round trip    200       701 ms   claude-haiku-4-5-20251001 answered

round trip over 3 probes: min 701 ms · mean 768 ms · max 826 ms

providers (/var/lib/tab-atelier-proxy/providers.json)
  deepseek       enabled
    round trip    401       985 ms   {"error":{"message":"Authentication Fails, ...
```

Each configured provider is probed with its OWN credential. The stages above
answer "is Anthropic reachable"; this answers "does the second provider work",
which is the question an operator has just created by adding one — and the one
that otherwise stays invisible until a reroute fails mid-turn.
```

Three stages because they fail for different reasons: a slow **credential**
stage is the OAuth refresh endpoint, not the API; **connect** is DNS, TCP and
TLS with no model work in it; **round trip** is a real one-token completion,
which is the number people actually wait for. It exits non-zero if any stage
fails, so it works as a monitoring probe and not only by eye.

It costs a handful of tokens against the shared plan. That is the price of
measuring the thing that matters — a HEAD to some unrelated path would time
the CDN and tell you nothing.

## Web UI

Browse to the proxy and paste the admin token:

```sh
sudo -u tab-atelier-proxy tab-atelier-proxy admin-token
# or, always correct and needing no binary:
sudo cat /var/lib/tab-atelier-proxy/admin.token
```

The first form works because the CLI finds a packaged install when the caller
has no data of its own. That matters: the systemd unit sets
`TAB_ATELIER_PROXY_CONFIG`, and a command run by hand inherits none of the
unit's environment — without that fallback the CLI reads a different directory
and mints a SECOND token, which authenticates nothing and looks exactly like a
wrong password.

Vue and Bootstrap are served by the proxy itself, with no CDN: a credential
proxy is exactly the kind of thing that runs on a locked-down network, and an
admin UI that needs outbound internet would be useless there — quite apart from
giving a third party a script tag on the page where the admin token is typed.

## Pointing a tab-atelier at it

On the machine with the terminal:

```sh
tab-atelier remote add --label proxy --url https://proxy.example.org --relay-token tap_…
tab-atelier relay via proxy
tab-atelier relay on
```

Claude tabs opened after that route through the proxy.

## `catbus-agent` is a client too

`catbus-agent` talks to the proxy the same way a claude tab does — it is a
relay client, not a second implementation of the login.

```sh
catbus-agent --relay-url https://proxy.example.org --relay-token tap_…
```

with no flags it reads the relay endpoint out of the same
`~/.config/tab-atelier/preferences.json` the app uses, so on a machine that
already runs tab-atelier a plain `catbus-agent` is enough. `CATBUS_RELAY_URL`,
`CATBUS_RELAY_TOKEN` and `CATBUS_PREFERENCES` are the environment equivalents.

The login lives **only** here. `catbus-agent` no longer reads
`~/.claude/.credentials.json`, no longer refreshes an OAuth token, and cannot
be pointed straight at `api.anthropic.com`: with no relay configured it refuses
to start rather than quietly going direct. Three consequences worth knowing:

- **It works on a box with no `claude` login at all.** A CI runner or a
  container only needs the relay token. There is nothing to refresh and no
  account to leak, because the client holds no subscription credential.
- **Compaction is the proxy's.** Reasoning models routed through the relay can
  emit `thinking` blocks; they are kept verbatim in the transcript and echoed
  back unchanged (the upstream requires its own reasoning blocks back), but
  they are never shown as the answer.
- **The Claude Code identifier in `system[0]` is still sent by the client**,
  because the relay forwards system blocks untouched and the upstream rejects a
  request without it. That string is not a credential — it is a request shape.

## Migrating from `relay egress`

The old role is refused now, with a message pointing here — deliberately, since
a relay that keeps half-working after its credential model changed is worse
than one that tells you what to install. On the box that was the egress:

1. `apt install tab-atelier-proxy`
2. add an account per person, hand out the keys
3. on each client, replace the shared token: `remote add … --relay-token <their key>`

The old shared token stops being anything at all — there is nowhere left that
accepts it.

## Where state lives

`/var/lib/tab-atelier-proxy` (0700), holding `users.json` and `admin.token`,
both 0600. Purging the package does **not** delete it: removing a package
should not silently revoke everyone's access with no way back.

## See also

- [Proxy-side request compaction](proxy-compaction.md) — what shortening
  `messages[]` costs and saves on each hop, and the per-provider control for it.
- [Proxy-side tool policy](proxy-tools.md) — `tools[]` is 19 % of a request and
  is ahead of every cache breakpoint: whitelist it, disable it, or rewrite the
  volatile parts of it. The two providers disagree about what is legal here.
