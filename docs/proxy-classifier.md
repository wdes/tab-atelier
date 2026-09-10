<!-- SPDX-License-Identifier: MPL-2.0 -->

# The auto-mode permission classifier

Claude Code's auto mode does not decide on its own whether a gated action may
run. It asks. For each `Bash`, `Edit` and the rest, it makes a **second,
separate Messages call** whose `system` prompt is Anthropic's security-monitor
prompt, whose single user turn is the transcript plus a `cc_automode_permissions`
block, and whose output is one tag the client *parses* rather than reads:

```json
{
  "model": "claude-haiku-4-5-20251001",
  "max_tokens": 64,
  "stop_sequences": ["</severity>"],
  "thinking": { "type": "disabled" },
  "system": [
    { "text": "You are Claude Code, Anthropic's official CLI for Claude." },
    { "text": "You are a security monitor for autonomous AI coding agents. …" }
  ],
  "messages": [
    { "role": "user", "content": "<transcript>…</transcript>\n<cc_automode_permissions>Bash</cc_automode_permissions>" }
  ]
}
```

`</severity>` is the tell. No other caller of this API sets it: it is only there
because the caller means to find the end of the verdict.

## Why the proxy treats it apart

None of the above is visible in the transcript, so the proxy used to route the
classifier as ordinary work — which meant four decisions were being taken
implicitly, by whatever the money-saving defaults happened to say:

1. **A cost mapping silently picked a judge.** An operator writing
   `opus → deepseek-flash` said something about the *conversation*. They did
   not say "let deepseek-flash adjudicate whether `rm -rf` is safe", and there
   is no way to read the second statement out of the first. A mapping is a
   statement about work; the classifier is the gate that decides whether work
   runs at all.
2. **Anthropic's own monitor prompt was relayed verbatim** to whichever third
   party the mapping named, once per gated action.
3. **The tool policy** ([`proxy-tools.md`](proxy-tools.md)) would have
   *injected* a `tools[]` into a body that has none — handing the judge a
   toolkit it was never written to hold.
4. **Call counts and token averages counted the judge as work**, so one gated
   action read as two calls and the per-account figures answered a question
   nobody asked.

## What changes, and what deliberately does not

| | Work | Classifier |
|---|---|---|
| Retargeted by a mapping | yes | **no** — routed on the name it asked for |
| Honours an account's provider pin | yes | yes (a pin is explicit about destination) |
| Walks the class ladder when nothing has capacity | yes | yes (it goes somewhere fast and cheap) |
| Compacted (`compact.rs`) | yes | **no** |
| `tools[]` injected by the tool policy | yes | **no** |
| Admitted through the QoS scheduler | yes | **yes** |
| Counted as work in the panel | yes | **labelled** `auto-mode` |

The classifier is routed on the model name the client sent. The pin and the
degrade ladder below still apply, so it lands on something cheap — it is just
not *the mapping* that decides which model judges.

Two things are deliberately not exemptions:

**It is still admitted through QoS.** It looks tiny — `max_tokens: 64`, one tag
out — but its *input* is the whole transcript, re-sent once per gated action,
so it spends the subscription for real. Exempting it would be a budget hole
dressed as a courtesy.

**It is still counted**, but labelled, so the per-account figures can be read as
work rather than inflated by the judge.

## The load-bearing field

Probing DeepSeek's Anthropic-compatible endpoint with this body:

| request | result |
|---|---|
| `thinking: {"type":"disabled"}` | `stop_reason: end_turn`, 9 output tokens, `<severity>safe</severity>` |
| without it, on `deepseek-flash` | `stop_reason: max_tokens`, 64 output tokens, **empty content** |

`deepseek-flash` reasons, and with a 64-token budget and no instruction to stop
it spends the entire budget thinking and never emits a verdict. Claude Code then
prints *"auto mode cannot determine the safety of Bash right now"* and the
action is not gated — which is exactly the message
[`brain.rs`](../src/cli/brain.rs) recognises and presses through.

Nothing in the proxy touches `thinking`, and a forwarding test asserts it stays
true. The point is that **anything** that strips, rewrites or defaults that field
on classifier traffic breaks auto mode for every gated action, and the symptom
looks like an upstream outage rather than a routing bug.

## Where it is detected

`crates/tab-atelier-proxy/src/classifier.rs`. Two signals, either sufficient:

* `</severity>` in `stop_sequences` (the contract — the output is parsed);
* Anthropic's monitor prompt in `system` (text or block form).

Both structural, not heuristic, and asymmetric on purpose: a body that merely
*contains* the words "security monitor" in a user message — someone asking why
auto mode is failing — is work and costs nothing, while a missed classifier is
exempted from nothing and leaves the old behaviour. False negatives are the safe
direction.

## See also

* [`proxy-compaction.md`](proxy-compaction.md) — the pass this is exempt from.
* [`proxy-tools.md`](proxy-tools.md) — the tool policy this is exempt from.
* [`proxy.md`](proxy.md) — the relay as a whole.
