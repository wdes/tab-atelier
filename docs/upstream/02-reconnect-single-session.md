# 02 — Reconnect started one session per press

**Upstreamable: in part.** The queue fix is generic and can go upstream. The transport close is
specific to tab-atelier and must stay here.

## What was wrong

Two things combined, and both are visible in the output as the same text written again at
different positions, which Ctrl+L hides until more output arrives.

1. `TerminalManager.requestReconnect` added the bridge to `pendingReconnect` on every call.
   While the network was down, each press of Reconnect queued the bridge again. When the
   network returned, `reconnectPending` started the bridge once per queued entry.
2. `TerminalBridge.startConnection` replaced `transport` without ending the previous one. A
   tab-atelier session still redialling kept its relay reading into the same emulator, so the
   new relay wrote a second copy of the replay.

## The change

- `TerminalManager.requestReconnect`: queue a bridge only if it is not already queued.
- `TerminalBridge.startConnection`: before replacing the transport, close it if it is a
  tab-atelier transport: `(transport as? TabAtelier)?.close()`.
- Regression test `TerminalManagerBellTest.reconnectingWhileOfflineStartsTheSessionOnce`.

## How to upstream it

**The queue fix (`requestReconnect`):** offer it as its own pull request. The guard
`pendingReconnect.none { it.get() === bridge }` is generic. Write the test in
`TerminalManager`'s own test class rather than the bell test this fork uses. Upstream's
`TerminalManager` has changed since the base (8 commits), so check that the function still
looks the same before porting.

**The close (`startConnection`):** do not offer it. Upstream's SSH transport defers `close()`
during a network grace period, so ending the previous transport generically would change SSH
behaviour. If upstream wants a generic version, the first step is an `AbsTransport` method
that says whether closing a replaced transport is safe, with SSH answering no.

## Upstream churn

`TerminalBridge.kt` changed upstream 15 times since the base, and `TerminalManager.kt` 8 times.
Both need a hand merge in a sync, and the reconnect code is the part most likely to conflict.
