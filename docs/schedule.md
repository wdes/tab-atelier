# Off-hours auto-lock (Schedule)

A tab can carry a *schedule*: a `(rule, tz)` pair that flips the tab
into the same locked state used by the manual right-click lock,
*automatically*, outside the rule's open windows.

The rule grammar is [OSM `opening_hours`][osm] — the same string you
might see on `<openstreetmap.org>` tagging shop hours. One string
covers weekly patterns, lunch breaks, public holidays, and night
shifts that cross midnight.

[osm]: https://wiki.openstreetmap.org/wiki/Key:opening_hours

## What "locked" means

When the schedule is closed, every **write** is refused with HTTP 423:

- `POST /input` — typing in the share-link viewer / mobile remote
- `POST /files` — inbox uploads from the viewer's drag-and-drop
- `POST /lock {"on": false}` — refusing manual unlock outside hours

**Reads stay open** so the viewer still receives `/output`, `/stream`,
and `/view`. The viewer banner shows *"locked until Mo 09:00
Europe/Paris"* with the next-change time computed from the rule.

Manual lock during open hours still wins — you can lock a tab during
work hours and it stays locked. You just can't *unlock* outside the
schedule's open windows; the schedule is the boundary, not a polite
suggestion.

## Timezone is mandatory

`Mo-Fr 09:00-18:00` alone is ambiguous on a server in a different
locale than the user. The schedule is always stored as
`{ rule, tz }` and the CLI / GUI require both. Use IANA names:
`Europe/Paris`, `America/New_York`, `Asia/Tokyo`, `UTC`.

## CLI — `tab-atelier schedule`

Both binaries expose the subcommand:

```sh
# Set: rule + tz
tab-atelier schedule 0 "Mo-Fr 09:00-18:00" --tz Europe/Paris
tab-atelier-headless schedule 0 "Mo-Fr 09:00-18:00" --tz Europe/Paris

# Clear (tab returns to always-open)
tab-atelier schedule 0 --clear

# Tab can be addressed by numeric index OR uuid
tab-atelier schedule 7b3a8e21-... "Mo-Fr 09:00-18:00; PH off" --tz Europe/Paris
```

The CLI calls `POST /tabs/by-id/{uuid}/schedule` with a JSON body
`{"rule": "...", "tz": "..."}`. Rule parsing happens server-side; a
bad rule or unknown tz is rejected with HTTP 400 and the parser's
error string surfaces on stderr.

## GUI — right-click tab menu

The Schedule presets live in the tab's right-click menu alongside the
background-color presets. Five presets pre-bound to the system tz
(read from `/etc/timezone`):

| Preset | Rule |
|---|---|
| Workdays 9-18 | `Mo-Fr 09:00-18:00` |
| Workdays 9-18 + lunch | `Mo-Fr 09:00-12:30,13:30-18:00` |
| Workdays 9-18 (no holidays) | `Mo-Fr 09:00-18:00; PH off` |
| Always open | `24/7` |
| Night shift 22-06 | `Mo-Fr 22:00-06:00` |

For anything outside the presets (different rule, different tz),
use the CLI — it's the same backend.

## Preset rules — examples

### Work patterns

| Rule | Meaning |
|---|---|
| `Mo-Fr 09:00-18:00` | Standard office hours |
| `Mo-Fr 09:00-12:30,13:30-18:00` | Office with lunch break |
| `Mo-Th 09:00-18:00; Fr 09:00-16:00` | Short Fridays |
| `Mo-Fr 08:00-17:00; PH off` | Office hours, locked on public holidays |
| `Mo,We,Fr 09:00-18:00` | 3-day week (Mon/Wed/Fri only) |
| `Mo-Fr 10:00-19:00; Sa 10:00-14:00` | 5.5-day week with Saturday morning |

### Focus / boundary patterns

| Rule | Meaning |
|---|---|
| `Mo-Fr 09:00-12:00` | Morning focus block only |
| `Mo-Fr 14:00-18:00` | Afternoon-only access |
| `Mo-Fr 09:00-18:00; Sa,Su off` | Hard weekend boundary |
| `Mo-Su 06:00-22:00` | Always closed overnight, 7 days |

### Always / never

| Rule | Meaning |
|---|---|
| `24/7` | Always open |
| `Mo-Su off` | Always locked |

### Shift patterns

| Rule | Meaning |
|---|---|
| `Mo-Fr 09:00-18:00; Sa-Su 10:00-14:00` | Weekdays full, weekend check-ins |
| `Mo-Fr 22:00-06:00` | Night shift (crosses midnight) |

## Wire format

Persisted in `tabs.json` as an optional field on each tab:

```json
{
  "id": "7b3a8e21-…",
  "name": "claude",
  "locked": false,
  "schedule": {
    "rule": "Mo-Fr 09:00-18:00; PH off",
    "tz": "Europe/Paris"
  }
}
```

Missing field = no schedule (tab is always-open). Old `tabs.json`
files deserialize cleanly — the field is `Option<TabSchedule>` with
`#[serde(default)]`.

## Response headers on `/output` and `/stream`

When a schedule is active, every poll carries:

| Header | Example | Meaning |
|---|---|---|
| `X-Tab-Locked` | `1` | Set whenever effective lock is on (manual or schedule) |
| `X-Tab-Locked-Reason` | `schedule` \| `manual` | Distinguishes the two |
| `X-Tab-Schedule-Tz` | `Europe/Paris` | IANA name to format the next-change time in |
| `X-Tab-Schedule-Next` | `2026-06-08T07:00:00+00:00` | RFC 3339 UTC instant of the next state flip |
| `X-Tab-Schedule-Rule` | `Mo-Fr%2009%3A00-18%3A00` | Percent-encoded rule string, for the viewer banner |

The viewer reads these on every poll, so a schedule transition at
18:00 reflects in the locked banner within one poll interval (~1 s)
— no F5 needed.

## Edge cases

| Case | Behaviour |
|---|---|
| Active session at close time | PTY keeps running. Lock only blocks writes; output keeps streaming. |
| Manual unlock outside hours | Refused with `423 Locked`. Remove the schedule first if you want full access. |
| Invalid rule on save | Rejected with `400`. No partial state — the schedule is unchanged. |
| Public holidays | `PH off` resolves against the schedule's tz country (Paris → FR). Crate-managed holidays table. |
| Daylight transitions | Handled by `chrono-tz`. Schedule fires at the wall-clock local-time mark — spring-forward gaps resolve via "earliest valid instant". |
| Headless restart | Schedule re-evaluated on first request; no enforcer task. State is fully derived. |

## Implementation

See `src/schedule.rs`. Two helpers drive every gate:

- `effective_locked(manual_locked, schedule)` — single source of truth
  for write gates.
- `lock_reason(manual_locked, schedule)` — `"manual"` / `"schedule"` /
  `None`, used by the response headers.

The schedule is **derived state**, not materialised. There's no
background enforcer task; each API request evaluates `is_open_now()`
lazily against the current wall-clock. Cost is a sub-microsecond
opening-hours parser call per gate hit.
