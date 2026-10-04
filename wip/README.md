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

Still needed before it can be applied:

- **PGPREV/PGNEXT should page the local scrollback**, not send PageUp to the
  remote — that is what JuiceSSH's buttons do, and it is why they are useful for
  reading output that has gone past. It needs a `ScrollController` in
  `ConsoleScreen`, obtained from `Terminal(onScrollControllerAvailable = …)` the
  same way the `ComposeController` already is, plus a page height.
- **Tests.** `TerminalKeyboardContentTest` was never extended for the new layout,
  so it has no coverage. Assert the keys, that `FN` reveals F1–F12 and hides the
  main page, that the back button returns — and assert the buttons are
  *clickable*, not merely present. Two controls shipped broken in this project
  because a test checked presence: a control can be present, visible, enabled and
  still unusable.

To work on it, apply it against `connectbot/` (the patch was written when the app
was reached through an overlay directory, so the paths may need `-p1` adjusted),
and please delete this directory once the work lands — a `wip/` that outlives its
contents is just another place code goes to be forgotten.
