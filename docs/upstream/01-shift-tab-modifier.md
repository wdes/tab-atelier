# 01 — Shift, Ctrl and Alt were dropped from the Tab and Esc keys

**Upstreamable: yes, as a standalone fix. This does not depend on tab-atelier.**

## What was wrong

In `service/TerminalKeyListener.kt`, the keys bar's Esc and Tab sent their key with a
modifier value of `0`, then cleared the modifier state. Every other key on the bar honoured
the latched modifier. So a latched Shift was eaten by the key after it: Shift then Tab sent a
plain Tab. Ctrl and Alt behaved the same way.

## The change

Both calls pass the modifier state the rest of the bar already uses:

```kotlin
-        keyDispatcher.dispatchKey(0, VTermKey.ESCAPE)
+        keyDispatcher.dispatchKey(modifiersForTerminal, VTermKey.ESCAPE)
-        keyDispatcher.dispatchKey(0, VTermKey.TAB)
+        keyDispatcher.dispatchKey(modifiersForTerminal, VTermKey.TAB)
```

Two comment lines were added with a `Changed for Tab Atelier Remote` marker. Remove the marker
when offering the change upstream; keep the reasoning comment, which says why the value matters.

## How to upstream it

1. Branch from upstream `HEAD` (not from this fork).
2. Apply the two-line change and the comment, without the fork marker.
3. Add a regression test beside `TerminalKeyListenerTest.kt`: latch Shift, send Tab, and assert
   the dispatched modifier includes Shift. Send Ctrl and Alt the same way.
4. Run upstream's own checks (`ci-build.yml`) before opening the pull request.

## Upstream churn

`TerminalKeyListener.kt` changed upstream once since the base. Read that upstream commit before
porting, since it may already touch the same two calls.
