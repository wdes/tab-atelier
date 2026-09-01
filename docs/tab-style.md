# Per-project tab colours and badges

Give a project a colour and a short badge, and every tab working in it looks
like it belongs there — in the tab strip, in the terminal itself, and on
`/tabs` for the mobile remote.

```
tab-atelier style --folder ~/Dev/kalpin --color '#7a1f2b' --badge KAL
tab-atelier style --list
tab-atelier style --folder ~/Dev/kalpin --clear
```

## Why the rule is keyed by folder

A new tab inherits the active tab's working directory, so keying the identity
on the folder means a tab opened with Ctrl+Shift+T is already the right colour
— nothing is copied from tab to tab, it's re-derived from where the tab is.
A tab that `cd`s into another project re-derives it too, on the next tick.

Rules match by path component, longest first, so a sub-project refines its
parent instead of fighting it:

| cwd | rule that wins |
| --- | --- |
| `~/Dev/app` | `~/Dev/app` |
| `~/Dev/app/src/deep` | `~/Dev/app` |
| `~/Dev/app/frontend` | `~/Dev/app/frontend` |
| `~/Dev/app-legacy` | none — `app-legacy` is not under `app` |

## Per-tab overrides

One tab can opt out of its project's look — the reviewer among five builders,
say:

```
tab-atelier style --tab 3 --color '#123456' --badge REV
tab-atelier style --tab 3 --clear     # back to the folder rule
```

Resolution is **per-tab override → folder rule → global default**
(`bg-color --global`, else Tomorrow Night Blue). Badges have no global level:
no override and no rule means no badge.

## Where it shows up

- **Tab strip** — the badge as a small tinted chip before the tab name.
- **Terminal background** — the tint replaces the theme's background for that
  tab. Cells with their own background colour are unaffected, so only the empty
  parts of the screen take the tint.
- **`GET /tabs`** — `badge`, plus the already-existing effective `bg_color`, so
  the share-link viewer and the Android remote render the same identity.

## Open question — should Ctrl+P match on the badge?

The MRU switcher filters on the tab name. Once projects carry a badge, typing
`OS` or `MOUNCH` there is an obvious thing to reach for, and today it finds
nothing unless the name happens to contain it.

Undecided, and worth using the feature for a while first: a badge is short and
shared by many tabs, so matching it would make one keystroke select 20 rows —
useful as a filter ("show me the Onestaff tabs"), unhelpful if you were trying
to jump to one tab. If it lands, the shape is probably to match name first and
fall back to badge, so a name hit always outranks a project-wide one.

## Storage

Folder rules live in `preferences.json` under `folder_styles` and are read at
daemon start, like `bg-color --global`:

```json
{
  "folder_styles": {
    "/home/w/Dev/kalpin": { "color": "#7a1f2b", "badge": "KAL" }
  }
}
```

An edit is picked up on the next tick — the daemon stats the file each tick and
re-reads it only when the mtime moved, so a rule change lands on a running
desktop without restarting it. Per-tab overrides go through the running daemon
(`POST /tabs/by-id/<id>/bg-color` and `/badge`) and are persisted in `tabs.json`
with the rest of the tab's state.
