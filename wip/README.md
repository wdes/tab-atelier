# wip/ — unfinished work, kept as patches

Nothing here is applied, compiled, or tested. `../README.md` and the build scripts
never look in this directory; a patch in here is a note to self, not part of the
app.

It exists because the alternative is throwing away work that was mostly done, or
leaving it half-applied in the tree so the build depends on something unfinished.
Neither is good: the first wastes the work, the second makes a broken build look
like a broken commit.

## 0007-terminal-keyboard-two-row-bar.patch

The JuiceSSH-style two-row, borderless special-keys bar, replacing the single
horizontally-scrolling row ConnectBot ships:

```
ESC   /     -     HOME ↑    END    PGPREV              FN
TAB   CTRL  ALT   ←         ↓      →        PGNEXT     ⌨
```

`FN` swaps the bar for an F1–F12 page whose trailing buttons are a back button
(`KeyboardReturn`) and paste.

It compiles and its structure is sound. Two bugs found reviewing it are already
fixed inside the patch: a missing closing brace that made every declaration after
it local (so the helper buttons "did not exist"), and `KeyButton` requiring a
`contentDescription` even when its visible label already names it.

Where it actually stands (2026-10-05), after applying it to the current tree:

**It compiles and the wiring is done.** The patch now also carries the call-site
change it needs — `ConsoleScreen` passes `onPaste = onPasteRequest` where it used to
pass `onOpenTextInput = onTextInputRequest`, since the new bar has no text-input
parameter. Nothing is lost by that swap: the title bar already has both a text-input
button and a paste button, so the dialog is still one tap away.

**PGPREV/PGNEXT cannot be made to scroll the local scrollback, and that is final for
this library version.** They send PageUp/PageDown, as the current bar does. Two
independent blockers, both checked against termlib 0.3.10 rather than assumed:

1. `onScrollControllerAvailable` is a parameter of `TerminalWithAccessibility`, **not
   of `Terminal`** — and `Terminal` is what the app calls. There is no overload
   carrying it.
2. `org.connectbot.terminal.ScrollController` is **`internal`** to the library, so
   app code cannot reference the type at all, let alone hold one in `remember`.

So the JuiceSSH behaviour those buttons are for needs a change in termlib or a
different rendering path — not a change here. Do not spend another round looking for
a way in: this was looked for exhaustively.

**Five existing tests assert the old bar and must be rewritten, not loosened.** They
expect an `IME` key, a `Text input` button and F1 on the main page, none of which the
new layout has (F1–F12 are behind `FN`). That much is expected staleness.

**But two of the failures are not staleness, and are the reason this is still parked.**
`terminalKeyboardContent_displaysCoreKeysAndInvokesCallbacks` fails with "Failed to
inject touch input", and `imeVisibleInvokesHideKeyboard` reports a click that never
fired its callback — while the toggle's wiring (`imeVisible` → `onHideIme`, else
`onShowIme`) is provably correct in the source. Both point the same way: a click that
does not reach a button that exists. The likely cause is the trailing column — `FN`
and the keyboard toggle — being laid out outside the area the click can reach, which
on a *narrow phone* would mean the two buttons that reach the second page and the
keyboard are unreachable. That is a product bug, not a test problem, and loosening
the assertions would hide it.

**So the next step is to measure, not to edit the tests**: put the bar on a device (or
in a screenshot/robolectric layout test with a phone-sized viewport) and check that
every key is inside the visible width, including the trailing column on the narrowest
supported screen. Fix the layout if it is not, then rewrite the tests against the
layout that survives. The new bar has no compose-mode key either, so whatever
`onToggleComposeMode` does for this app needs a home before the old bar can go.

