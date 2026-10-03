<!-- SPDX-License-Identifier: MPL-2.0 -->

# Three resolvers disagree about what names a tab

**Status: reported, not decided.** Every fix here is user-visible, so the
choice is not one to make as a side effect of tidying.

Three functions turn a key a person typed into one tab. They were written for
different verbs at different times, and they do not agree.

| | `share_link::resolve` | `team::resolve_target` | `remote::resolver::pick_tab` |
|---|---|---|---|
| used by | `close`, `rename`, `lock`, `input`, `stats`, … | `note`, `peek`, `dispatch --to` | `remote attach/put/get` |
| precedence | index → uuid → name | **name** → index → uuid | `#`index → uuid → name |
| bare `3` | index 3 | index 3, *unless a tab is named `3`* | never an index — a name or uuid |
| `#3` | no match | no match | index 3 |
| name match | case-sensitive | case-sensitive | **case-insensitive** |
| index base | 0-based | 0-based | 0-based |
| two tabs share a name | error, lists indexes | error, lists indexes | error, suggests `#<index>` |

## What agrees

**The index base.** All three compare against the `index` field the API
publishes, which is `.enumerate()` over the tab list in `src/api/tabs.rs:41` —
0-based, one definition, no arithmetic in between. There is no off-by-one
between them, which was the first thing worth ruling out.

**The collision rule.** None of the three guesses between two tabs sharing a
name; all three refuse and say how to disambiguate. Acting on the wrong tab
closes or types into somebody else's work, and every author reached the same
conclusion independently.

## What diverges, and what it costs

1. **Precedence.** A tab named `3` is reachable by `note --to 3` but shadows
   index 3 for that verb, while `close 3` always means the index. The same
   string means two different tabs depending on the verb.
2. **The `#` prefix.** `remote get box 3` looks up a tab *named* `3`, where
   `close 3` is an index. A person who learns one form gets silence — "no tab
   matched" — rather than an error explaining the other form exists.
3. **Case.** `remote attach box BUILD` finds a tab named `build`; `close BUILD`
   does not.

## Why this is not just fixed

Unifying means choosing, and both choices break something:

- **Bare number = index** (the majority) breaks anyone whose tabs are named
  after ticket numbers, silently: they get a different tab, not an error.
- **Bare number = name first** (team's rule) means `close 3` can stop meaning
  index 3 the moment somebody names a tab `3`.
- **Case-folding names** merges tabs that are currently distinct, which turns a
  working command into an ambiguity error.

`remote`'s `#N` is arguably the right design — explicit, no collision possible
— but adopting it everywhere breaks every script that says `close 0`.

`tests/tab_key_resolution.rs` pins the current behaviour of the two pure
resolvers, so the divergence cannot widen quietly while this is undecided. It
asserts what the code does today; it is not an endorsement of it.
