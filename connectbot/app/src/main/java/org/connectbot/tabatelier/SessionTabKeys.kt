/*
 * ConnectBot: simple, powerful, open-source SSH client for Android
 * Copyright 2025-2026 Kenny Root
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.connectbot.tabatelier

import java.util.concurrent.ConcurrentHashMap

/**
 * Which tab each host's session is on.
 *
 * Two questions, deliberately answered differently, because they are different questions:
 *
 * - **What tab was just asked for?** A *pending* choice. Consumed by the transport that opens
 *   the session, because it belongs to that one connection attempt.
 * - **What tab is this host on?** A *sticky* fact. Read by anything that has to reopen a
 *   session without a fresh choice — the app's own reconnect, a transport rebuilt to switch
 *   tabs, a session restored after the process died.
 *
 * ## Why the distinction is a class and not two maps in a service
 *
 * Because getting it wrong is invisible until a user reports it, and it was: the pending key
 * was consumed exactly once — correct for a choice — and the app's reconnect path
 * (`TerminalManager.requestReconnect` → `startConnection`) builds a **new** transport, so the
 * key taken by the old one was gone, `connect()` had nothing to open, and the session failed
 * with "no tab was chosen" for a tab the user had plainly tapped. A reconnect that cannot name
 * its tab is a reconnect that does not work.
 *
 * Extracted here so that rule has a test rather than only a comment: nothing in this class
 * needs a socket, a service or a running app.
 *
 * [ConcurrentHashMap] because a retry coroutine, a tab switch on the UI thread and the
 * manager's own bookkeeping can all reach it.
 */
class SessionTabKeys {
    private val pending = ConcurrentHashMap<Long, String>()
    private val last = ConcurrentHashMap<Long, String>()

    /**
     * Records the tab the user just chose.
     *
     * Written to both maps: the choice is pending for the connection about to open, and it is
     * also now the tab this host is on, whatever happens to that connection.
     */
    fun choose(hostId: Long, tabKey: String) {
        pending[hostId] = tabKey
        last[hostId] = tabKey
    }

    /**
     * Takes the pending choice, consuming it.
     *
     * Consuming is right: a choice belongs to one connection attempt, and a later attempt must
     * not silently inherit it. What it *must* inherit is [last], and that is written when the
     * choice is made rather than here — see [choose]. Writing it in both places would be
     * redundant, and redundant code that looks load-bearing is worse than either: an earlier
     * version did it here, which made the test for it pass with the line deleted.
     */
    fun takePending(hostId: Long): String? = pending.remove(hostId)

    /**
     * The tab this host is on, for an attempt that has no fresh choice.
     *
     * Called after [takePending] has returned null, which is exactly the reconnect case.
     */
    fun resumeKey(hostId: Long): String? = last[hostId]

    /** Records that a live session has moved to a tab, so a later reopen lands there. */
    fun remember(hostId: Long, tabKey: String) {
        last[hostId] = tabKey
    }

    /** Forgets a host entirely, for when its session is closed for good. */
    fun forget(hostId: Long) {
        pending.remove(hostId)
        last.remove(hostId)
    }

    /**
     * Drops a pending choice while keeping the host's current tab.
     *
     * The case this exists for: a tap put a choice in place, the session it was meant for
     * never started, and a live session was then told to switch directly. The leftover choice
     * belongs to nobody, and leaving it would send some later connection to a tab the user did
     * not ask for — but the switch that just happened *is* the host's current tab, so that
     * must survive.
     */
    fun forgetPending(hostId: Long) {
        pending.remove(hostId)
    }
}
