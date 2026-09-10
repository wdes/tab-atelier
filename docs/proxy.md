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

So it does two things, in order:

1. **Reroute.** Same class of model, different provider. Bedrock and Vertex
   serve the *same* models from *different* quota pools, so a saturated
   subscription is a reason to move the work, not to make it worse.
2. **Degrade**, only when nothing is left that can serve the class — a cheaper
   class, trading quality for getting an answer at all.

Never upward: idle capacity in an expensive class does not promote a request
that asked for something cheap. Never silently: the response carries
`x-tab-atelier-proxy-route: <provider>/<model>`, plus
`x-tab-atelier-proxy-rerouted` or `-degraded` naming what was asked for.

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
        {"id": "claude-haiku-4-5-20251001", "class": "fast",     "relative_cost": 1},
        {"id": "claude-sonnet-5",           "class": "balanced", "relative_cost": 5},
        {"id": "claude-opus-5",             "class": "heavy",    "relative_cost": 25}
      ]
    }
  ]
}
```

Add a second provider with `"auth": {"kind": "api_key_env", "var": "SOME_KEY"}`
and a higher `preference`. The key is named, not inlined — a credential in a
config file is a credential in a backup. A provider whose variable is unset is
never offered, because routing to it would produce a 401 from somewhere nobody
was looking.

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

It is off, and it turns itself off. A capture is a prompt, and a prompt is
whatever someone was working on, so:

* armed explicitly, for a stated number of minutes, up to 60;
* **it disarms itself** — "remember to switch it off" is not a control, and a
  debug flag left on is how a month of everyone's prompts ends up in a file;
* 40 captures, each clipped to 32 KB with both ends kept;
* `inspect.jsonl` beside the accounts, `0600`, and an armed window never
  survives a restart;
* admin-only. A user key cannot read captures, not even its own account's.

Credentials are removed before anything is written. Headers are an allowlist,
so `Authorization` and `x-api-key` are absent by construction rather than by a
rule someone could forget to update, and both bodies are additionally swept for
`sk-ant-…` and `tap_…` runs — because a prompt can contain a key that no header
rule would catch, and "why is my key not working, here it is" is exactly the
kind of session that gets inspected.

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
