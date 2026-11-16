# Upstreaming the ConnectBot changes

This fork is ConnectBot with Tab Atelier changes on top. These documents go through every
change in `connectbot/` and say which ones can be offered back to upstream, how, and which
ones cannot. They are the record `docs/connectbot-fork.md` points to.

## The two commits

- **Base (what the fork is built on):** upstream `e484bd3a` (`chore: remove unused strings`).
  Recorded by the squash commit's `git-subtree-split` trailer.
- **Upstream now:** upstream `1798d07` (`Fix disablePortForward KDoc param description`),
  186 commits after the base. Checked on 2026-11-16; the fork has **not** been synced to it.

## Groups

| Document | What it covers | Upstreamable? |
|---|---|---|
| [01-shift-tab-modifier.md](01-shift-tab-modifier.md) | Shift, Ctrl and Alt were dropped from Tab and Esc | **Yes**, as a standalone fix |
| [02-reconnect-single-session.md](02-reconnect-single-session.md) | Reconnect started one session per press; a replaced transport was not ended | **Part**: the queue fix yes; the transport close is tab-atelier only |
| [03-tab-atelier-protocol.md](03-tab-atelier-protocol.md) | The tab-atelier transport, its connection policy, editor fields, database columns | **No**: a new protocol, kept in this fork |
| [04-branding-and-removals.md](04-branding-and-removals.md) | Renamed app, removed Contact screen, deep-link scheme, strings | **No** |
| [05-build-and-versioning.md](05-build-and-versioning.md) | Gradle versioning, OkHttp, cleartext traffic | **No**, and cleartext needs revisiting |

## Upstream churn per file

`Upstream commits since base` is how many upstream commits touched the file between the base
and `1798d07`. Any non-zero count means a sync has to merge that file by hand.

| Upstream commits since base | File (under `connectbot/`) | Here |
|---:|---|---|
| 55 | `gradle/libs.versions.toml` | modified |
| 26 | `app/src/main/res/values/strings.xml` | modified |
| 25 | `app/build.gradle.kts` | modified |
| 19 | `app/src/main/java/org/connectbot/ui/screens/console/ConsoleScreen.kt` | modified |
| 15 | `app/src/main/java/org/connectbot/service/TerminalBridge.kt` | modified |
| 11 | `app/src/main/java/org/connectbot/data/ConnectBotDatabase.kt` | modified |
| 8 | `app/src/main/java/org/connectbot/service/TerminalManager.kt` | modified |
| 8 | `app/src/main/java/org/connectbot/ui/screens/hosteditor/HostEditorScreen.kt` | modified |
| 7 | `app/src/main/java/org/connectbot/ui/components/TerminalKeyboard.kt` | modified |
| 7 | `app/src/main/java/org/connectbot/ui/navigation/NavGraph.kt` | modified |
| 6 | `app/src/main/java/org/connectbot/ui/screens/console/ConsoleViewModel.kt` | modified |
| 6 | `app/src/main/java/org/connectbot/ui/screens/hostlist/HostListScreen.kt` | modified |
| 5 | `app/src/main/java/org/connectbot/data/entity/Host.kt` | modified |
| 5 | `app/src/main/java/org/connectbot/ui/screens/hosteditor/HostEditorViewModel.kt` | modified |
| 5 | `app/src/test/kotlin/org/connectbot/TerminalKeyboardContentTest.kt` | modified |
| 5 | `app/src/test/kotlin/org/connectbot/ui/screens/hosteditor/HostEditorViewModelTest.kt` | modified |
| 4 | `app/src/main/java/org/connectbot/ui/MainActivity.kt` | modified |
| 4 | `app/src/test/kotlin/org/connectbot/ui/screens/console/ConsoleViewModelTest.kt` | modified |
| 3 | `app/src/test/kotlin/org/connectbot/HostListScreenTest.kt` | modified |
| 2 | `app/src/main/AndroidManifest.xml` | modified |
| 2 | `app/src/main/java/org/connectbot/ui/navigation/NavDestinations.kt` | modified |
| 2 | `app/src/main/java/org/connectbot/ui/screens/hostlist/HostListViewModel.kt` | modified |
| 1 | `app/schemas/org.connectbot.data.ConnectBotDatabase/12.json` | added here |
| 1 | `app/schemas/org.connectbot.data.ConnectBotDatabase/13.json` | added here |
| 1 | `app/src/main/java/org/connectbot/service/TerminalKeyListener.kt` | modified |
| 1 | `app/src/main/java/org/connectbot/ui/ConnectBotApp.kt` | modified |
| 1 | `app/src/main/java/org/connectbot/ui/screens/help/HelpScreen.kt` | modified |
| 1 | `app/src/test/kotlin/org/connectbot/ui/screens/hostlist/HostListViewModelTest.kt` | modified |
| 0 | `app/src/main/assets/adi-registration.properties` | added here |
| 0 | `app/src/main/assets/connectbot-LICENSE.txt` | added here |
| 0 | `app/src/main/java/org/connectbot/service/ConnectionState.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/service/TabAtelierConnectionPolicy.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/ConnectionEventListener.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/ConnectionProgress.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/SessionTabKeys.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/TabAtelierAddresses.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/TabAtelierClient.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/tabatelier/TabAtelierTab.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/transport/TabAtelier.kt` | added here |
| 0 | `app/src/main/java/org/connectbot/transport/Transport.kt` | modified |
| 0 | `app/src/main/java/org/connectbot/ui/screens/contact/ContactScreen.kt` | deleted here |
| 0 | `app/src/main/res/values-fr/strings_tabatelier.xml` | added here |
| 0 | `app/src/main/res/values/notrans.xml` | modified |
| 0 | `app/src/main/res/values/strings_tabatelier.xml` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/ContactScreenTest.kt` | deleted here |
| 0 | `app/src/test/kotlin/org/connectbot/HelpScreenTest.kt` | modified |
| 0 | `app/src/test/kotlin/org/connectbot/service/TabAtelierConnectionPolicyTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/service/TerminalManagerTabSwitchTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/service/TerminalQueryReplyTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/tabatelier/ConnectionEventListenerTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/tabatelier/ConnectionProgressTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/tabatelier/SessionTabKeysTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/tabatelier/TabAtelierAddressesTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/tabatelier/TabAtelierClientTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/transport/TabAtelierTest.kt` | added here |
| 0 | `app/src/test/kotlin/org/connectbot/ui/screens/console/TerminalColorTest.kt` | added here |

## Reproducing the table

```sh
git clone https://github.com/connectbot/connectbot.git upstream
git -C upstream log --oneline e484bd3a..1798d07 -- <path under connectbot/>
# and the diff against the base, from this repository:
git diff e484bd3a -- connectbot/        # needs e484bd3a in this clone's history
```

## Decision still open: moving the base forward

Upstream is 186 commits ahead. Moving the base to `1798d07` means merging those commits into
`connectbot/`, and the 56 files above would conflict in the places they overlap. The fork has
not been moved, and the base in `NOTICE` and the About screen is still `e484bd3a`. A sync is a
separate change, best made after the upstreamable fixes (group 01 and the queue half of group
02) have been accepted upstream, so that the sync does not have to carry them.
