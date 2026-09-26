# The output-token ceiling, and what a truncated reply actually costs

Measured **2026-09-26** against the DeepSeek relay (`deepseek-flash`, Anthropic-shaped
endpoint). Everything in the tables below is a real request and a real response; the two
figures marked *modelled* are arithmetic on those measurements, and say so.

## The symptom

A long answer arrived cut off, with a note from catbus:

```
[reply cut off at the 8192-token output limit — it may end mid-sentence; ask for the rest to continue]
```

The note was the whole problem: it made a broken reply look like a supported feature. The
operator asks for the rest, and the model has to resume a sentence whose beginning it can no
longer see — the cut reply is in history, but what followed it was the operator's words, not
the model's own train of thought.

## Where 8192 came from

The client. `MAX_OUTPUT_TOKENS` was 8192, sent as `max_tokens` on every request. It was never
a provider limit:

| `max_tokens` sent | response |
| --- | --- |
| 16384 | `200` `end_turn` |
| 65536 | `200` `end_turn` |
| 393216 | `200` `end_turn` |

DeepSeek documents a 384K maximum output and reports `max_tokens` as fully supported on this
endpoint; the relay in front of it does not clamp. The old ceiling was ~47× below what the
endpoint allows.

## Measuring a real truncation

Prompt: *"Print the integers from 1 to 8000, one per line."* Round A is the old behaviour —
ceiling 8192, then continue, exactly as the client did it. Round B is one request with room to
finish.

```
A1 (cap 8192):  stop=max_tokens  in=51    cache_read=0  out=8192    chars=9773
A2 (continue):  stop=max_tokens  in=5665  cache_read=0  out=8192    chars=0
B1 (cap 32768): stop=end_turn    in=51    cache_read=0  out=31405   chars=38892
```

Three things to read out of that.

**The continuation was a cache miss, essentially all of it.** `cache_read=0` on a 5665-token
input. Two reasons, and only one of them is general: the partial answer is new text that has
never been sent as *input* before, so it cannot hit; and the 51-token prompt is below whatever
minimum this cache holds, so it did not hit either. In a real session the conversation prefix
*does* hit — measured below — which is why the cost of a retry is set by the size of the partial
answer, not by the size of the conversation.

**Round two produced no text at all** — 8192 output tokens spent, `chars=0`. Reasoning and the
answer share one budget here; `budget_tokens` is ignored on this endpoint, so there is no
separate allowance to reason within. A turn whose thinking runs long can consume the entire
ceiling and return nothing. That is a better account of the original complaint than length is:
the reply was truncated because *thinking* reached the ceiling, not because the answer was long.

**The answer wanted 31405 tokens.** At 8192 that is four round-trips.

## What a kind of retry costs

Prices for `deepseek-flash`, per 1M tokens, off-peak → peak:

| | off-peak | peak |
| --- | --- | --- |
| output | $0.60 | $1.20 |
| input, cache miss | $0.15 | $0.30 |
| input, cache hit | $0.003 | $0.006 |

An identical prompt sent twice — 6038 tokens — hits cache on the second send:

```
1st: in=6038  cache_read=0
2nd: in=150   cache_read=5888
3rd: in=150   cache_read=5888
```

So a stable conversation prefix is nearly free to re-send; note that no `cache_control` was
sent, and the hit happened anyway. Caching on this endpoint is automatic, which matches
DeepSeek's documentation that `cache_control` is ignored — catbus's cache breakpoints are inert
here. What is *not* covered by that is the partial answer, which is new input every time (see
above).

Which gives the two rules that decide the ceiling:

- **A high ceiling is free.** A reply is billed for the output tokens it produces, not for the
  ceiling it was allowed. The model stops when it is done either way.
- **A retry is not.** It re-sends the partial answer at miss price and it costs a round-trip.

Measured, on this one task: finishing in one request cost **$0.0189** and produced the whole
answer (38892 characters). The truncated run cost **$0.0107** and produced a quarter of it
(9773 characters) — the second round, given the same 8192, produced no text at all. *Modelled*,
producing the same answer at a ceiling of 8192 takes **at least** four rounds and about
**$0.026**, roughly **+39%** over finishing in one; at least, because a round that yields no
text (as round two above did) is paid for without shortening the job.

A ceiling is also what bounds a reply that has gone wrong:

| ceiling | worst-case single reply |
| --- | --- |
| 32768 | $0.020 |
| 65536 | $0.039 |
| 393216 | $0.24 |

## The decision

`MAX_OUTPUT_TOKENS` is **65536**.

It covers the measured worst case (31405) with room over it, and — because thinking draws on the
same budget — room for a turn that thinks hard before answering. It is not the provider's 384K:
a reply that goes badly wrong should be cut off somewhere, and 65536 bounds that at a few cents.

The continuation path stays, and is now a safety net rather than the mechanism. It should rarely
fire; when it does, it costs what the tables above say.

## What this does not fix

- **The OpenAI-format path still asks for 8192**, hardcoded with no knob
  (`crates/catbus-agent/src/openai.rs`, already noted in `docs/audit-findings.md`). It is not
  simply raised, because on that path a large `max_tokens` is refused by models with a lower cap
  — it wants a per-model setting, not a bigger constant. The Anthropic path is now 65536 but
  likewise has no flag, only the constant.
- **Nothing here contains process execution.** A test that spawns is still stopped by the
  `PHPUnit` function block, which is a different guard for a different problem.

## How this was measured

Against the relay, reading credentials from the operator's own
`~/.config/tab-atelier/preferences.json`. Regenerate any table above with:

```sh
python3 - <<'PY'
import json, urllib.request as U, urllib.error
d = json.load(open('/home/williamdes/.config/tab-atelier/preferences.json'))
e = next(x for x in d['remote_endpoints'] if x.get('relay_token'))
url = e['url'].rstrip('/') + '/relay/anthropic/v1/messages'
h = {'x-api-key': e['relay_token'], 'anthropic-version': '2023-06-01', 'content-type': 'application/json'}
for n in (16384, 65536, 393216):
    b = {'model': 'deepseek-flash', 'max_tokens': n, 'messages': [{'role': 'user', 'content': 'say ok'}]}
    req = U.Request(url, data=json.dumps(b).encode(), headers=h, method='POST')
    try:
        s = U.urlopen(req, timeout=180)
        j = json.loads(s.read())
        print(n, '->', s.status, j.get('stop_reason'), j.get('usage'))
    except urllib.error.HTTPError as x:
        print(n, '-> ERR', x.code, str(x.read())[:200])
PY
```
