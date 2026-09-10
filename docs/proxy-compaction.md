<!-- SPDX-License-Identifier: MPL-2.0 -->

# Proxy-side request compaction

The proxy already reads every request body whole. `shape_and_admit` parses it to
a `serde_json::Value` to find `model`, decides where the call goes, and
`rewrite_model` re-serializes it. Compaction is one more mutation on a parse the
proxy is already paying for — no new endpoint, no new credential, no client
change.

It is also, on any hop that still has a warm cache, **worth much less than the
byte count says**. Most of this document is the argument for why.

## What it is worth, measured

A Claude Code request is not a short question. It is the *entire conversation*,
resent every turn, with `cache_control` breakpoints placed so that everything
before the last one is a cache read at ~10% of list price.

The render order is `tools` → `system` → `messages`, and **any byte change
anywhere in the prefix invalidates every breakpoint after it.** So a compactor
that rewrites the middle of `messages` turns that turn into a full-price cache
miss. Shorter body, bigger bill — for one turn.

A real body, measured (77 messages, 306,158 bytes, bound for `deepseek-flash`):

| where the breakpoints are | what that means |
|---|---|
| `system[1]`, `system[2]` | the system prompt and tool-definition tail are cached |
| `messages[76].content[0]` | **the last message** — so the *entire* history is inside the cached prefix |

`tools[]` carries **no** breakpoint, so every tool sits ahead of all three; the
same byte changed in the tail costs nothing, and changed in a tool description
costs everything. That is `docs/proxy-tools.md`'s subject.

**The obvious inference from that table is wrong.** An Anthropic prompt cache
does not follow a request to a provider that is not Anthropic — so it looks like
a reroute forfeits the cache, and *that* is the free hop. It is not. Probed
against the second provider's Anthropic-compatible endpoint, the real session
was served from cache on its 77th turn:

```
input_tokens 195   cache_read_input_tokens 77,312   cache_creation 0
```

77,312 of 77,507 tokens read from cache, ~195 uncached — the new message only.
**This endpoint prefix-caches a Claude Code session**, and does not honour the
client's breakpoint structure while doing it: it matches its own longest prefix.
The same body, compacted, and both re-sent warm:

| | before | after | |
|---|---|---|---|
| body bytes | 306,158 | 174,777 | **−43 %** |
| input tokens (measured, not `len/4`) | 77,507 | 45,640 | **−41.1 %** |
| `tool_result` (43 blocks) | 129,896 B | 21,836 B | 34 of 43 elided |
| `thinking` (25 blocks) | 27,799 B | 6,343 B | 19 dropped, last 6 kept |
| `tool_use` / `text` / `system[]` / `tools[]` | — | identical | untouched |

So the framing is not "the cache is gone, the bytes are free". It is this:

```
baseline   195 + 0.1 × 77,312  =  7,926  effective
compacted  200 + 0.1 × 45,440  =  4,744  effective      →  −40 %
```

**The percentage survives; the money does not.** A cache scales both sides
equally, so 41 % off the tokens is still 41 % off the input bill — but the bill
being discounted is already a tenth of what a cold request costs, so the
absolute saving is roughly **10× smaller than the byte count advertises.**

That is still a saving, and on a provider whose whole reason for existing is
price it is a real one. It is just not the slam dunk a −43 % figure reads as,
and the decision belongs per-provider for exactly that reason: the operator is
choosing where 40 % of a small number beats the cache churn and the semantic
loss below, not where 43 % of a large one does.


## The three layers

Deterministic — identical input bytes produce identical output bytes, so the
rewrite happens once and the cache (if any) re-warms on that one turn instead of
missing forever. That rules out anything time-based or random.

| # | layer | what it does | kept verbatim |
|---|-------|--------------|---------------|
| A | `tools` | replaces old `tool_result` **content** with a stub naming the byte count and `tool_use_id` | last **6** tool-result turns |
| B | `+thinking` | drops `thinking` blocks on older assistant turns | last **6** assistant turns |
| C | `+banners` | drops the stale `<total_tokens>…</total_tokens>` banner messages Claude Code re-injects each turn | the newest one |

The stubs are the point of layer A: `"[tool result elided by tab-atelier-proxy:
9073 bytes; tool_use_id=call_00_Xv1cviAjZqN6HSYLdu3g4425]"` keeps the block, its
id and its position, and tells the model *that something was there* rather than
pretending it was always empty.

Layer C is worth almost nothing in bytes — 23 messages × 49 B ≈ 1.2 KB. It is
worth keeping for a different reason: a stale token count re-read 23 times is
noise, not context.

## The control

A `<select>` on each provider in the admin UI, written to that provider's entry
in `providers.json`:

```html
<select v-model="provider.compact">
  <option value="none">None</option>
  <option value="tools">Remove old tool results</option>
  <option value="tools_thinking">Remove old tool results and thinking</option>
  <option value="all">Remove old tool results, thinking and banners</option>
</select>
```

Parsed as a field on the provider, defaulting to `"none"` for providers already
in the file:

```json
{
  "id": "deepseek",
  "base_url": "https://api.deepseek.com/anthropic",
  "compact": "tools_thinking"
}
```

**`none` is the default, and it is the right value on the Anthropic provider.**
The whole point is that the operator sets it per hop, with the reasoning above
visible in the same file the routing already reads.

Worth saying plainly: a global `compact: "all"` on every provider would be a
configuration that quietly costs its owner money. If a global control is ever
added it should refuse to apply to a provider whose `auth.kind` is
`claude_oauth` — a setting that cannot be correct is a setting that should not
be offerable.

## What must never change

Verified on the body above, before and after:

- **`tool_use` ↔ `tool_result` pairing.** 43 ids on each side, the sets
  *identical* in both versions. Elision replaces content, never the block — a
  dropped `tool_result` leaves its `tool_use` unanswered, which is a 400.
- **The tail.** The 6 kept `thinking` blocks and the 9 kept `tool_result` blocks
  are byte-identical to the originals.
- **`system[]`, `tools[]`, `metadata`, `context_management`, `thinking`,
  `output_config`.** All hash identical. The cache root and the tool schema are
  not in the blast radius.

The last one is not decoration: `tools[]` sits at the front of the cache prefix,
and editing a schema can desync the `tool_use` arguments the model already
emitted.

## Where it hooks in

In `shape_and_admit` ([`server.rs`](../crates/tab-atelier-proxy/src/server.rs)),
after `routing::choose` has picked a destination and before `qos::estimate_cost`
scores it — so the admission decision is made at the size that will actually be
sent. The body is already a `Value` at that point, and `rewrite_model` is the
pattern to follow.

The policy itself lives in the proxy crate, in `src/compact.rs`, beside
[`src/transcript_compact.rs`](../src/transcript_compact.rs) in the sense of
being the same *idea* — that module implements layers equivalent to B and A
(its `keep_thinking(K)` and `tool_cap(N)`) over the on-disk JSONL.

They cannot share code, and it is worth being explicit about why, because the
temptation is real. The input shapes differ (a stored transcript versus a
request body), the operation differs (a byte cap on a tool output there,
whole-block elision with a stub here), and the proxy does not depend on the
desktop crate — doing so would drag a GUI toolkit into a server. What the two
*do* share is the one invariant that must not drift: **a `tool_use` and its
`tool_result` are a pair, and a dropped `tool_result` leaves its `tool_use`
unanswered, which is a 400.**

## The honest limits

**Elided errors are not just bulk.** In the measured body a 454-byte `PreToolUse`
hook error — *"use the Grep tool instead of shelling out to `grep`"* — was
elided. It is older than the 6-turn window, so the model has moved past it, but
it is the one elision class where the loss is semantic rather than size. The
safe rule is to keep every `is_error: true` result regardless of age, at a cost
of a few KB.

**This is not context editing.** Anthropic's own `context_management.edits`
(`clear_tool_uses_20250919`, `clear_thinking_20251015`) does the same clearing
**server-side**, cache-aware, with the pairing rules enforced by the thing that
defines them. Claude Code already sends
`{"keep":"all","type":"clear_thinking_20251015"}` — it declares the mechanism and
then tells it to keep everything. Tightening that edit is a one-field change
with a fraction of the blast radius of rewriting the body, and it should be the
first thing tried on a provider that honours it. This document describes the
fallback for the providers that do not.

**And it is not free of the cache argument even here.** Compact early and often
and you re-warm a truncated prefix on every route change. The setting is
per-provider precisely so that decision stays where it was made.

## See also

- [Proxy-side tool policy](proxy-tools.md) — the same chokepoint and the same
  per-provider control applied to `tools[]`, which is 19 % of the request and
  sits *ahead of* every breakpoint this document reasons about.
