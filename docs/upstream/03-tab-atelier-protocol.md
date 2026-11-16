# 03 — The tab-atelier protocol

**Upstreamable: no.** This is a new transport for a server this project runs. It stays in
this fork. This document lists the pieces so a sync knows what to keep.

## What it is

A tab-atelier host is a server that serves terminal tabs over HTTP and a WebSocket. The app
attaches to one tab and replays its output. It is a viewer: the tab keeps running on the
server, and a dropped connection redials.

## The pieces

| Area | Files |
|---|---|
| Transport | `transport/TabAtelier.kt` (new), `transport/Transport.kt` (a `TabAtelier` object) |
| Client and addresses | `tabatelier/TabAtelierClient.kt`, `tabatelier/TabAtelierAddresses.kt`, `tabatelier/TabAtelierTab.kt` |
| Connection state and progress | `service/ConnectionState.kt`, `service/TabAtelierConnectionPolicy.kt`, `tabatelier/ConnectionProgress.kt`, `tabatelier/ConnectionEventListener.kt` |
| Tab selection | `tabatelier/SessionTabKeys.kt`, `service/TerminalManager.kt` (`switchTab`, tab keys), `ui/navigation/NavGraph.kt` (optional tab argument), `ui/MainActivity.kt` (pending tab) |
| Host data | `data/entity/Host.kt` (`tabAtelierUrl`, `tabAtelierUrls`), `data/ConnectBotDatabase.kt` and schemas 12 and 13 (migrations) |
| Editor and list | `ui/screens/hosteditor/*`, `ui/screens/hostlist/*`, `ui/screens/console/*` |
| Strings | `res/values/strings_tabatelier.xml`, `res/values-fr/strings_tabatelier.xml` |

## Why it cannot be upstreamed

A protocol is a product decision for the upstream maintainers, and this one only makes sense
against a server that is not part of ConnectBot. Offering it would put a dependency on
tab-atelier into an app that does not use it.

## What a sync must keep

- The database migrations (12 and 13). They are versioned after upstream's last schema, so an
  upstream migration with the same number would collide. Renumber them when upstream adds one.
- `Host.kt` fields. The fork's schema JSON in `app/schemas` must be regenerated after a sync.
- The strings file is a separate resource, because upstream's `strings.xml` changes often
  (26 upstream commits since the base). Keep new strings in `strings_tabatelier.xml`, not in
  upstream's file.

## Notes on the editor

`HostEditorScreen` and `HostEditorViewModel` were changed in several places, and both are
among the most-changed upstream files (8 and 5 upstream commits). A sync should merge the
fields and the validation separately from the layout.
